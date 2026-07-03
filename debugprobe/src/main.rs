#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    eprintln!("rustadmin-debugprobe is Windows-only.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    if let Err(err) = windows_app::run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_app {
    use std::{
        collections::HashSet,
        env,
        ffi::{OsStr, OsString},
        fs::{self, File},
        io::{self, BufWriter, Read, Write},
        mem,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        ptr,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use winapi::{
        shared::{
            minwindef::{BOOL, DWORD, FALSE, LPARAM, TRUE, WPARAM},
            windef::HWND,
        },
        um::{
            errhandlingapi::GetLastError,
            fileapi::{CreateFileW, CREATE_ALWAYS},
            handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
            processthreadsapi::OpenProcess,
            tlhelp32::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            winnt::{
                GENERIC_WRITE, HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_VM_READ,
            },
            winuser::{
                EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
                IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_NULL,
            },
        },
    };

    const MINI_DUMP_WITH_HANDLE_DATA: u32 = 0x0000_0004;
    const MINI_DUMP_WITH_UNLOADED_MODULES: u32 = 0x0000_0020;
    const MINI_DUMP_WITH_THREAD_INFO: u32 = 0x0000_1000;

    #[link(name = "dbghelp")]
    extern "system" {
        fn MiniDumpWriteDump(
            process: HANDLE,
            process_id: DWORD,
            file: HANDLE,
            dump_type: u32,
            exception_param: *mut std::ffi::c_void,
            user_stream_param: *mut std::ffi::c_void,
            callback_param: *mut std::ffi::c_void,
        ) -> BOOL;
    }

    pub fn run() -> io::Result<()> {
        let options = Options::parse()?;
        if options.help {
            print_help();
            return Ok(());
        }

        fs::create_dir_all(&options.out_dir)?;
        let mut logger = JsonLogger::new(&options.out_dir.join("events.jsonl"))?;
        logger.event_fields(
            "probe_start",
            vec![
                ("out_dir", json_string(&options.out_dir.display().to_string())),
                ("duration_ms", options.duration.as_millis().to_string()),
                ("interval_ms", options.interval.as_millis().to_string()),
                ("dump", options.dump.to_string()),
                ("user", json_string(&env::var("USERNAME").unwrap_or_default())),
                ("computer", json_string(&env::var("COMPUTERNAME").unwrap_or_default())),
            ],
        )?;

        let user_log = env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("RustAdmin")
            .join("log");
        let service_log =
            PathBuf::from(r"C:\WINDOWS\ServiceProfiles\LocalService\AppData\Roaming\RustAdmin\log");

        log_command(&mut logger, &options.out_dir, "whoami", "whoami", &[])?;
        log_command(
            &mut logger,
            &options.out_dir,
            "tasklist-rustadmin",
            "tasklist",
            &["/v", "/fi", "imagename eq RustAdmin.exe"],
        )?;
        log_command(
            &mut logger,
            &options.out_dir,
            "tasklist-rustadmin-lower",
            "tasklist",
            &["/v", "/fi", "imagename eq rustadmin.exe"],
        )?;
        log_command(
            &mut logger,
            &options.out_dir,
            "sc-query-rustadmin",
            "sc",
            &["queryex", "RustAdmin"],
        )?;

        logger.event_fields(
            "file_latency",
            vec![
                ("name", json_string("out_dir")),
                ("result", file_latency_json(&options.out_dir)),
            ],
        )?;
        logger.event_fields(
            "file_latency",
            vec![
                ("name", json_string("user_log")),
                ("result", file_latency_json(&user_log)),
            ],
        )?;

        copy_recent_logs(&mut logger, &user_log, &options.out_dir.join("user-log-before"))?;
        copy_recent_logs(
            &mut logger,
            &service_log,
            &options.out_dir.join("service-log-before"),
        )?;

        let start = Instant::now();
        let mut sample_index = 0_u64;
        while start.elapsed() < options.duration {
            sample_index += 1;
            let processes = rustadmin_processes();
            let process_pids: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
            let windows = rustadmin_windows(&process_pids);
            let pipes = rustadmin_pipes();
            logger.event_fields(
                "sample",
                vec![
                    ("index", sample_index.to_string()),
                    ("elapsed_ms", start.elapsed().as_millis().to_string()),
                    ("processes", process_list_json(&processes)),
                    ("windows", window_list_json(&windows)),
                    ("pipes", string_array_json(&pipes)),
                ],
            )?;
            logger.flush()?;
            thread::sleep(options.interval);
        }

        if options.dump {
            let dump_dir = options.out_dir.join("dumps");
            fs::create_dir_all(&dump_dir)?;
            for process in rustadmin_processes() {
                let dump_path = dump_dir.join(format!("{}-{}.dmp", process.name, process.pid));
                let result = write_minidump(process.pid, &dump_path);
                logger.event_fields(
                    "dump",
                    vec![
                        ("pid", process.pid.to_string()),
                        ("name", json_string(&process.name)),
                        ("path", json_string(&dump_path.display().to_string())),
                        ("result", result),
                    ],
                )?;
            }
        }

        copy_recent_logs(&mut logger, &user_log, &options.out_dir.join("user-log-after"))?;
        copy_recent_logs(
            &mut logger,
            &service_log,
            &options.out_dir.join("service-log-after"),
        )?;

        let summary = options.out_dir.join("summary.txt");
        write_summary(&summary, &options.out_dir)?;
        logger.event_fields(
            "probe_stop",
            vec![
                ("out_dir", json_string(&options.out_dir.display().to_string())),
                ("summary", json_string(&summary.display().to_string())),
            ],
        )?;
        logger.flush()?;

        println!("RustAdmin debug probe completed.");
        println!("Output: {}", options.out_dir.display());
        Ok(())
    }

    struct Options {
        out_dir: PathBuf,
        duration: Duration,
        interval: Duration,
        dump: bool,
        help: bool,
    }

    impl Options {
        fn parse() -> io::Result<Self> {
            let mut out_dir = None;
            let mut duration = Duration::from_secs(30);
            let mut interval = Duration::from_millis(1000);
            let mut dump = false;
            let mut help = false;

            let mut args = env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--out" => out_dir = Some(PathBuf::from(next_arg(&mut args, "--out")?)),
                    "--duration" => duration = Duration::from_secs(parse_next(&mut args, "--duration")?),
                    "--interval-ms" => interval = Duration::from_millis(parse_next(&mut args, "--interval-ms")?),
                    "--dump" => dump = true,
                    "--help" | "-h" => help = true,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown argument '{arg}'"),
                        ));
                    }
                }
            }

            let out_dir = out_dir.unwrap_or_else(default_out_dir);
            Ok(Self { out_dir, duration, interval, dump, help })
        }
    }

    fn print_help() {
        println!(
            "rustadmin-debugprobe\n\
             \n\
             Options:\n\
               --duration SECONDS     default 30\n\
               --interval-ms MS       default 1000\n\
               --out DIR              default Desktop\\rustadmin-debug-<timestamp>\n\
               --dump                 write minidumps of RustAdmin processes\n"
        );
    }

    fn default_out_dir() -> PathBuf {
        let desktop = env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Desktop");
        desktop.join(format!("rustadmin-debug-{}", timestamp_for_path()))
    }

    fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> io::Result<String> {
        args.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{name} requires a value"))
        })
    }

    fn parse_next<T>(args: &mut impl Iterator<Item = String>, name: &str) -> io::Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let value = next_arg(args, name)?;
        value.parse::<T>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {name} value '{value}': {err}"),
            )
        })
    }

    #[derive(Clone)]
    struct ProcessInfo {
        pid: u32,
        parent_pid: u32,
        thread_count: u32,
        name: String,
    }

    #[derive(Clone)]
    struct WindowInfo {
        hwnd: usize,
        pid: u32,
        title: String,
        responsive: bool,
        elapsed_ms: u128,
        error: u32,
    }

    fn rustadmin_processes() -> Vec<ProcessInfo> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Vec::new();
            }
            let mut entry: PROCESSENTRY32W = mem::zeroed();
            entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as DWORD;
            let mut out = Vec::new();
            let mut ok = Process32FirstW(snapshot, &mut entry);
            while ok == TRUE {
                let name = wide_z_to_string(&entry.szExeFile);
                if name.eq_ignore_ascii_case("RustAdmin.exe") || name.eq_ignore_ascii_case("rustadmin.exe") {
                    out.push(ProcessInfo {
                        pid: entry.th32ProcessID,
                        parent_pid: entry.th32ParentProcessID,
                        thread_count: entry.cntThreads,
                        name,
                    });
                }
                ok = Process32NextW(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
            out
        }
    }

    fn rustadmin_windows(process_pids: &HashSet<u32>) -> Vec<WindowInfo> {
        struct Context {
            pids: HashSet<u32>,
            windows: Vec<WindowInfo>,
        }

        unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let context = unsafe { &mut *(lparam as *mut Context) };
            if unsafe { IsWindowVisible(hwnd) } == FALSE {
                return TRUE;
            }
            let mut pid = 0_u32;
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid); }
            if !context.pids.contains(&pid) {
                return TRUE;
            }

            let title = unsafe { window_title(hwnd) };
            let mut result = 0_usize;
            let start = Instant::now();
            let ret = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_NULL,
                    0 as WPARAM,
                    0 as LPARAM,
                    SMTO_ABORTIFHUNG,
                    500,
                    &mut result,
                )
            };
            let elapsed_ms = start.elapsed().as_millis();
            let error = if ret == 0 { unsafe { GetLastError() } } else { 0 };
            context.windows.push(WindowInfo {
                hwnd: hwnd as usize,
                pid,
                title,
                responsive: ret != 0,
                elapsed_ms,
                error,
            });
            TRUE
        }

        let mut context = Context { pids: process_pids.clone(), windows: Vec::new() };
        unsafe { EnumWindows(Some(enum_window), &mut context as *mut _ as LPARAM); }
        context.windows
    }

    unsafe fn window_title(hwnd: HWND) -> String {
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0_u16; len as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        OsString::from_wide(&buf[..copied.max(0) as usize]).to_string_lossy().into_owned()
    }

    fn rustadmin_pipes() -> Vec<String> {
        match fs::read_dir(r"\\.\pipe\") {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.to_ascii_lowercase().contains("rustadmin"))
                .collect(),
            Err(err) => vec![format!("pipe-list-error: {err}")],
        }
    }

    fn file_latency_json(dir: &Path) -> String {
        if !dir.exists() {
            return format!("{{\"exists\":false,\"path\":{}}}", json_string(&dir.display().to_string()));
        }
        let path = dir.join(format!("debugprobe-io-{}.tmp", timestamp_for_path()));
        let payload = vec![b'x'; 4096];
        let start = Instant::now();
        let write_result = fs::write(&path, &payload);
        let write_ms = start.elapsed().as_millis();
        if let Err(err) = write_result {
            return format!(
                "{{\"exists\":true,\"ok\":false,\"path\":{},\"write_ms\":{},\"error\":{}}}",
                json_string(&dir.display().to_string()),
                write_ms,
                json_string(&err.to_string())
            );
        }

        let start = Instant::now();
        let mut read_ok = false;
        if let Ok(mut file) = File::open(&path) {
            let mut data = Vec::new();
            read_ok = file.read_to_end(&mut data).is_ok();
        }
        let read_ms = start.elapsed().as_millis();

        let start = Instant::now();
        let delete_ok = fs::remove_file(&path).is_ok();
        let delete_ms = start.elapsed().as_millis();

        format!(
            "{{\"exists\":true,\"ok\":true,\"path\":{},\"write_ms\":{},\"read_ms\":{},\"read_ok\":{},\"delete_ms\":{},\"delete_ok\":{}}}",
            json_string(&dir.display().to_string()),
            write_ms,
            read_ms,
            read_ok,
            delete_ms,
            delete_ok
        )
    }

    fn copy_recent_logs(logger: &mut JsonLogger, source: &Path, target: &Path) -> io::Result<()> {
        fs::create_dir_all(target)?;
        if !source.exists() {
            logger.event_fields(
                "logs_missing",
                vec![
                    ("source", json_string(&source.display().to_string())),
                    ("target", json_string(&target.display().to_string())),
                ],
            )?;
            return Ok(());
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, entry.path()));
            }
        }
        files.sort_by_key(|(modified, _)| *modified);
        files.reverse();

        let mut copied = 0_u32;
        for (_, path) in files.into_iter().take(12) {
            if let Some(name) = path.file_name() {
                let dest = target.join(name);
                if fs::copy(&path, dest).is_ok() {
                    copied += 1;
                }
            }
        }
        logger.event_fields(
            "logs_copied",
            vec![
                ("source", json_string(&source.display().to_string())),
                ("target", json_string(&target.display().to_string())),
                ("copied", copied.to_string()),
            ],
        )?;
        Ok(())
    }

    fn log_command(logger: &mut JsonLogger, out_dir: &Path, name: &str, program: &str, args: &[&str]) -> io::Result<()> {
        let output_path = out_dir.join(format!("command-{name}.txt"));
        let start = Instant::now();
        let output = Command::new(program).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).output();
        let elapsed_ms = start.elapsed().as_millis();

        match output {
            Ok(output) => {
                let mut file = BufWriter::new(File::create(&output_path)?);
                file.write_all(&output.stdout)?;
                if !output.stderr.is_empty() {
                    file.write_all(b"\n--- stderr ---\n")?;
                    file.write_all(&output.stderr)?;
                }
                file.flush()?;
                logger.event_fields(
                    "command",
                    vec![
                        ("name", json_string(name)),
                        ("program", json_string(program)),
                        ("elapsed_ms", elapsed_ms.to_string()),
                        ("status", output.status.code().unwrap_or(-1).to_string()),
                        ("path", json_string(&output_path.display().to_string())),
                    ],
                )?;
            }
            Err(err) => {
                logger.event_fields(
                    "command_error",
                    vec![
                        ("name", json_string(name)),
                        ("program", json_string(program)),
                        ("elapsed_ms", elapsed_ms.to_string()),
                        ("error", json_string(&err.to_string())),
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn write_minidump(pid: u32, path: &Path) -> String {
        unsafe {
            let process = OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                FALSE,
                pid,
            );
            if process.is_null() {
                return format!("{{\"ok\":false,\"error\":\"OpenProcess failed: {}\"}}", GetLastError());
            }

            let wide_path = wide_null(path.as_os_str());
            let file = CreateFileW(
                wide_path.as_ptr(),
                GENERIC_WRITE,
                0,
                ptr::null_mut(),
                CREATE_ALWAYS,
                0,
                ptr::null_mut(),
            );
            if file == INVALID_HANDLE_VALUE {
                let err = GetLastError();
                CloseHandle(process);
                return format!("{{\"ok\":false,\"error\":\"CreateFileW failed: {err}\"}}");
            }

            let dump_type = MINI_DUMP_WITH_HANDLE_DATA | MINI_DUMP_WITH_UNLOADED_MODULES | MINI_DUMP_WITH_THREAD_INFO;
            let ok = MiniDumpWriteDump(process, pid, file, dump_type, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            let err = GetLastError();
            CloseHandle(file);
            CloseHandle(process);
            if ok == TRUE {
                "{\"ok\":true}".to_string()
            } else {
                format!("{{\"ok\":false,\"error\":\"MiniDumpWriteDump failed: {err}\"}}")
            }
        }
    }

    fn write_summary(path: &Path, out_dir: &Path) -> io::Result<()> {
        let mut file = BufWriter::new(File::create(path)?);
        writeln!(file, "RustAdmin debug probe")?;
        writeln!(file, "Output: {}", out_dir.display())?;
        writeln!(file, "Processes:")?;
        let processes = rustadmin_processes();
        for process in &processes {
            writeln!(file, "  pid={} ppid={} threads={} name={}", process.pid, process.parent_pid, process.thread_count, process.name)?;
        }
        let pids: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        writeln!(file, "Windows:")?;
        for window in rustadmin_windows(&pids) {
            writeln!(
                file,
                "  hwnd=0x{:X} pid={} responsive={} elapsed_ms={} title={}",
                window.hwnd, window.pid, window.responsive, window.elapsed_ms, window.title
            )?;
        }
        file.flush()
    }

    struct JsonLogger {
        writer: BufWriter<File>,
        seq: u64,
    }

    impl JsonLogger {
        fn new(path: &Path) -> io::Result<Self> {
            Ok(Self { writer: BufWriter::new(File::create(path)?), seq: 0 })
        }

        fn event_fields(&mut self, event: &str, fields: Vec<(&str, String)>) -> io::Result<()> {
            self.seq += 1;
            write!(self.writer, "{{\"ts_ms\":{},\"seq\":{},\"event\":{}", unix_ms(), self.seq, json_string(event))?;
            for (key, value) in fields {
                write!(self.writer, ",\"{key}\":{value}")?;
            }
            writeln!(self.writer, "}}")
        }

        fn flush(&mut self) -> io::Result<()> { self.writer.flush() }
    }

    fn process_list_json(processes: &[ProcessInfo]) -> String {
        let items: Vec<String> = processes.iter().map(|p| {
            format!(
                "{{\"pid\":{},\"parent_pid\":{},\"thread_count\":{},\"name\":{}}}",
                p.pid, p.parent_pid, p.thread_count, json_string(&p.name)
            )
        }).collect();
        format!("[{}]", items.join(","))
    }

    fn window_list_json(windows: &[WindowInfo]) -> String {
        let items: Vec<String> = windows.iter().map(|w| {
            format!(
                "{{\"hwnd\":\"0x{:X}\",\"pid\":{},\"title\":{},\"responsive\":{},\"elapsed_ms\":{},\"error\":{}}}",
                w.hwnd, w.pid, json_string(&w.title), w.responsive, w.elapsed_ms, w.error
            )
        }).collect();
        format!("[{}]", items.join(","))
    }

    fn string_array_json(values: &[String]) -> String {
        let items: Vec<String> = values.iter().map(|value| json_string(value)).collect();
        format!("[{}]", items.join(","))
    }

    fn json_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ch if ch.is_control() => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "\\u{:04x}", ch as u32);
                }
                _ => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    fn wide_z_to_string(value: &[u16]) -> String {
        let len = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
        OsString::from_wide(&value[..len]).to_string_lossy().into_owned()
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn unix_ms() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
    }

    fn timestamp_for_path() -> String {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string()
    }
}
