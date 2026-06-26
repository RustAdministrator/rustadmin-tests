#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    eprintln!("rustadmin-dxgi-probe is Windows-only.");
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
        ffi::OsStr,
        fs::File,
        io::{self, BufWriter, Write},
        mem,
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr, thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use winapi::{
        shared::{
            dxgi::{
                CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
                IDXGIResource, DXGI_ADAPTER_DESC1, DXGI_OUTPUT_DESC,
                DXGI_RESOURCE_PRIORITY_MAXIMUM,
            },
            dxgi1_2::{
                IDXGIOutput1, IDXGIOutputDuplication, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
            },
            minwindef::{DWORD, FALSE, UINT},
            ntdef::HRESULT,
            windef::{HBITMAP, HDC, HGDIOBJ, POINT, RECT},
            winerror::{DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT},
        },
        um::{
            d3d11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
            libloaderapi::GetModuleHandleW,
            unknwnbase::IUnknown,
            wingdi::{
                BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
                GetDIBits, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
                DIB_RGB_COLORS, SRCCOPY,
            },
            winuser::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetDC,
                GetDesktopWindow, RedrawWindow, RegisterClassW, ReleaseDC, SetCursorPos,
                SetLayeredWindowAttributes, SetWindowPos, ShowWindow, UpdateWindow, CS_HREDRAW,
                CS_VREDRAW, HWND_TOPMOST, LWA_ALPHA, RDW_ALLCHILDREN, RDW_INVALIDATE,
                RDW_UPDATENOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
        Interface,
    };

    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    pub fn run() -> io::Result<()> {
        let options = Options::parse()?;
        if options.help {
            print_help();
            return Ok(());
        }

        let displays = enumerate_displays()?;
        if options.list_displays {
            for (index, display) in displays.iter().enumerate() {
                println!(
                    "#{index}: adapter='{}' output='{}' rect={}x{}+{}+{} attached={}",
                    display.adapter_name,
                    display.output_name,
                    display.width(),
                    display.height(),
                    display.rect.left,
                    display.rect.top,
                    display.attached_to_desktop
                );
            }
            return Ok(());
        }

        let Some(display) = displays.get(options.display).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "display {} is unavailable; run --list-displays first",
                    options.display
                ),
            ));
        };

        let mut logger = JsonLogger::new(&options.jsonl)?;
        logger.event(
            "start",
            format!(
                "\"display\":{},\"duration_ms\":{},\"timeout_ms\":{},\"wake\":\"{}\",\"mode\":\"{}\",\"rect\":{{\"left\":{},\"top\":{},\"right\":{},\"bottom\":{}}}",
                options.display,
                options.duration.as_millis(),
                options.timeout_ms,
                options.wake.as_str(),
                options.mode.as_str(),
                display.rect.left,
                display.rect.top,
                display.rect.right,
                display.rect.bottom
            ),
        )?;

        if options.mode == Mode::HybridGdiBootstrap {
            match capture_gdi_snapshot(&display) {
                Ok(snapshot) => {
                    logger.event(
                        "gdi_bootstrap",
                        format!(
                            "\"width\":{},\"height\":{},\"stride\":{},\"hash\":\"{:016x}\"",
                            snapshot.width, snapshot.height, snapshot.stride, snapshot.hash
                        ),
                    )?;
                    if let Some(path) = options.save_first.as_ref() {
                        save_bgra_bmp(
                            path,
                            snapshot.width,
                            snapshot.height,
                            snapshot.stride,
                            &snapshot.data,
                        )?;
                    }
                }
                Err(err) => logger.event("gdi_bootstrap_error", json_error(&err))?,
            }
        }

        let mut capturer = DxgiCapturer::new(&display)?;
        logger.event(
            "dxgi_created",
            format!(
                "\"width\":{},\"height\":{},\"mode_desc_width\":{},\"mode_desc_height\":{},\"format\":{},\"rotation\":{}",
                capturer.width,
                capturer.height,
                capturer.desc.ModeDesc.Width,
                capturer.desc.ModeDesc.Height,
                capturer.desc.ModeDesc.Format,
                capturer.desc.Rotation
            ),
        )?;

        let mut wake_window = if options.wake == WakeMode::LayeredWindow {
            match WakeWindow::new() {
                Ok(window) => {
                    logger.event("wake_window_created", String::new())?;
                    Some(window)
                }
                Err(err) => {
                    logger.event("wake_window_error", json_error(&err))?;
                    None
                }
            }
        } else {
            None
        };

        let start = Instant::now();
        let mut attempts = 0_u64;
        let mut frames = 0_u64;
        let mut timeouts = 0_u64;
        let mut access_lost = 0_u64;
        let mut first_dxgi_saved = options.mode != Mode::DxgiOnly;

        while start.elapsed() < options.duration {
            attempts += 1;
            if options.wake != WakeMode::None {
                if let Some(window) = wake_window.as_mut() {
                    window.pulse();
                }
                apply_wake(options.wake);
            }

            let attempt_start = Instant::now();
            match capturer.frame(options.timeout_ms, options.map_pointer_only) {
                Ok(frame) => {
                    frames += 1;
                    let wait_ms = attempt_start.elapsed().as_millis();
                    logger.event(
                        "frame",
                        format!(
                            "\"attempt\":{},\"wait_ms\":{},\"accumulated_frames\":{},\"last_present_time\":{},\"last_mouse_update_time\":{},\"metadata_bytes\":{},\"pointer_shape_bytes\":{},\"width\":{},\"height\":{},\"row_pitch\":{},\"hash\":\"{:016x}\"",
                            attempts,
                            wait_ms,
                            frame.info.AccumulatedFrames,
                            large_integer_value(&frame.info.LastPresentTime),
                            large_integer_value(&frame.info.LastMouseUpdateTime),
                            frame.info.TotalMetadataBufferSize,
                            frame.info.PointerShapeBufferSize,
                            frame.width,
                            frame.height,
                            frame.row_pitch,
                            frame.hash
                        ),
                    )?;
                    if !first_dxgi_saved {
                        if let Some(path) = options.save_first.as_ref() {
                            save_bgra_bmp(
                                path,
                                frame.width,
                                frame.height,
                                frame.row_pitch,
                                &frame.data,
                            )?;
                        }
                        first_dxgi_saved = true;
                    }
                }
                Err(DxgiProbeError::Timeout) => {
                    timeouts += 1;
                    logger.event(
                        "timeout",
                        format!(
                            "\"attempt\":{},\"timeout_ms\":{}",
                            attempts, options.timeout_ms
                        ),
                    )?;
                }
                Err(DxgiProbeError::PointerOnly(info)) => {
                    logger.event(
                        "pointer_only",
                        format!(
                            "\"attempt\":{},\"accumulated_frames\":{},\"last_present_time\":{},\"last_mouse_update_time\":{},\"metadata_bytes\":{},\"pointer_shape_bytes\":{}",
                            attempts,
                            info.AccumulatedFrames,
                            large_integer_value(&info.LastPresentTime),
                            large_integer_value(&info.LastMouseUpdateTime),
                            info.TotalMetadataBufferSize,
                            info.PointerShapeBufferSize
                        ),
                    )?;
                }
                Err(DxgiProbeError::AccessLost(hr)) => {
                    access_lost += 1;
                    logger.event(
                        "access_lost",
                        format!("\"attempt\":{},\"hr\":\"0x{:08x}\"", attempts, hr as u32),
                    )?;
                    break;
                }
                Err(DxgiProbeError::Hresult(hr)) => {
                    logger.event(
                        "hresult_error",
                        format!("\"attempt\":{},\"hr\":\"0x{:08x}\"", attempts, hr as u32),
                    )?;
                    break;
                }
                Err(DxgiProbeError::Io(err)) => {
                    logger.event(
                        "io_error",
                        format!("\"attempt\":{},{}", attempts, json_error(&err)),
                    )?;
                    break;
                }
            }
        }

        logger.event(
            "summary",
            format!(
                "\"attempts\":{},\"frames\":{},\"timeouts\":{},\"access_lost\":{}",
                attempts, frames, timeouts, access_lost
            ),
        )?;
        logger.flush()?;

        println!(
            "DXGI probe done: attempts={attempts}, frames={frames}, timeouts={timeouts}, log={}",
            options.jsonl.display()
        );
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Mode {
        DxgiOnly,
        HybridGdiBootstrap,
    }

    impl Mode {
        fn parse(value: &str) -> io::Result<Self> {
            match value {
                "dxgi" => Ok(Self::DxgiOnly),
                "hybrid-gdi-bootstrap" => Ok(Self::HybridGdiBootstrap),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown mode '{value}'"),
                )),
            }
        }

        fn as_str(self) -> &'static str {
            match self {
                Self::DxgiOnly => "dxgi",
                Self::HybridGdiBootstrap => "hybrid-gdi-bootstrap",
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum WakeMode {
        None,
        Cursor,
        Redraw,
        LayeredWindow,
    }

    impl WakeMode {
        fn parse(value: &str) -> io::Result<Self> {
            match value {
                "none" => Ok(Self::None),
                "cursor" => Ok(Self::Cursor),
                "redraw" => Ok(Self::Redraw),
                "layered-window" => Ok(Self::LayeredWindow),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown wake mode '{value}'"),
                )),
            }
        }

        fn as_str(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::Cursor => "cursor",
                Self::Redraw => "redraw",
                Self::LayeredWindow => "layered-window",
            }
        }
    }

    struct Options {
        display: usize,
        duration: Duration,
        timeout_ms: u32,
        wake: WakeMode,
        mode: Mode,
        jsonl: PathBuf,
        save_first: Option<PathBuf>,
        map_pointer_only: bool,
        list_displays: bool,
        help: bool,
    }

    impl Options {
        fn parse() -> io::Result<Self> {
            let mut options = Self {
                display: 0,
                duration: Duration::from_secs(30),
                timeout_ms: 500,
                wake: WakeMode::None,
                mode: Mode::DxgiOnly,
                jsonl: PathBuf::from("dxgi-probe.jsonl"),
                save_first: None,
                map_pointer_only: false,
                list_displays: false,
                help: false,
            };

            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--display" => options.display = parse_next(&mut args, "--display")?,
                    "--duration" => {
                        let secs: u64 = parse_next(&mut args, "--duration")?;
                        options.duration = Duration::from_secs(secs);
                    }
                    "--timeout-ms" => options.timeout_ms = parse_next(&mut args, "--timeout-ms")?,
                    "--wake" => {
                        let value: String = parse_next(&mut args, "--wake")?;
                        options.wake = WakeMode::parse(&value)?;
                    }
                    "--mode" => {
                        let value: String = parse_next(&mut args, "--mode")?;
                        options.mode = Mode::parse(&value)?;
                    }
                    "--jsonl" => {
                        options.jsonl = PathBuf::from(parse_next::<String>(&mut args, "--jsonl")?);
                    }
                    "--save-first" => {
                        options.save_first = Some(PathBuf::from(parse_next::<String>(
                            &mut args,
                            "--save-first",
                        )?));
                    }
                    "--map-pointer-only" => options.map_pointer_only = true,
                    "--list-displays" => options.list_displays = true,
                    "--help" | "-h" => options.help = true,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown argument '{arg}'"),
                        ));
                    }
                }
            }

            Ok(options)
        }
    }

    fn parse_next<T>(args: &mut impl Iterator<Item = String>, name: &str) -> io::Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let Some(value) = args.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} requires a value"),
            ));
        };
        value.parse::<T>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {name} value '{value}': {err}"),
            )
        })
    }

    fn print_help() {
        println!(
            "rustadmin-dxgi-probe\n\
             \n\
             Options:\n\
               --list-displays\n\
               --display N                 default 0\n\
               --duration SECONDS          default 30\n\
               --timeout-ms MS             default 500\n\
               --wake none|cursor|redraw|layered-window\n\
               --mode dxgi|hybrid-gdi-bootstrap\n\
               --map-pointer-only          map S_OK frames even when LastPresentTime is 0\n\
               --save-first PATH           write first captured BGRA frame as BMP\n\
               --jsonl PATH                default dxgi-probe.jsonl\n"
        );
    }

    #[derive(Clone)]
    struct DisplayInfo {
        adapter_index: UINT,
        output_index: UINT,
        adapter_name: String,
        output_name: String,
        rect: RECT,
        attached_to_desktop: bool,
    }

    impl DisplayInfo {
        fn width(&self) -> u32 {
            (self.rect.right - self.rect.left).max(0) as u32
        }

        fn height(&self) -> u32 {
            (self.rect.bottom - self.rect.top).max(0) as u32
        }
    }

    struct DxgiCapturer {
        device: ComPtr<ID3D11Device>,
        context: ComPtr<ID3D11DeviceContext>,
        duplication: ComPtr<IDXGIOutputDuplication>,
        width: u32,
        height: u32,
        desc: DXGI_OUTDUPL_DESC,
    }

    struct CapturedFrame {
        info: DXGI_OUTDUPL_FRAME_INFO,
        width: u32,
        height: u32,
        row_pitch: u32,
        hash: u64,
        data: Vec<u8>,
    }

    enum DxgiProbeError {
        Timeout,
        PointerOnly(DXGI_OUTDUPL_FRAME_INFO),
        AccessLost(HRESULT),
        Hresult(HRESULT),
        Io(io::Error),
    }

    impl From<io::Error> for DxgiProbeError {
        fn from(value: io::Error) -> Self {
            Self::Io(value)
        }
    }

    impl DxgiCapturer {
        fn new(display: &DisplayInfo) -> io::Result<Self> {
            unsafe {
                let factory = create_factory()?;
                let adapter = get_adapter(factory.0, display.adapter_index)?;
                let output = get_output(adapter.0, display.output_index)?;
                let output1 = query_output1(output.0)?;

                let mut device = ptr::null_mut();
                let mut context = ptr::null_mut();
                let hr = D3D11CreateDevice(
                    adapter.0 as *mut IDXGIAdapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                    0,
                    D3D11_SDK_VERSION,
                    &mut device,
                    ptr::null_mut(),
                    &mut context,
                );
                hresult_to_io(hr, "D3D11CreateDevice")?;
                let device = ComPtr::new(device);
                let context = ComPtr::new(context);

                let mut duplication: *mut IDXGIOutputDuplication = ptr::null_mut();
                let hr = (*output1.0).DuplicateOutput(device.0 as *mut IUnknown, &mut duplication);
                hresult_to_io(hr, "DuplicateOutput")?;
                let duplication = ComPtr::new(duplication);

                let mut desc = mem::zeroed();
                (*duplication.0).GetDesc(&mut desc);

                Ok(Self {
                    device,
                    context,
                    duplication,
                    width: display.width(),
                    height: display.height(),
                    desc,
                })
            }
        }

        fn frame(
            &mut self,
            timeout_ms: u32,
            map_pointer_only: bool,
        ) -> Result<CapturedFrame, DxgiProbeError> {
            unsafe {
                let mut info: DXGI_OUTDUPL_FRAME_INFO = mem::zeroed();
                let mut resource = ptr::null_mut();
                let hr =
                    (*self.duplication.0).AcquireNextFrame(timeout_ms, &mut info, &mut resource);
                if hr == DXGI_ERROR_WAIT_TIMEOUT {
                    return Err(DxgiProbeError::Timeout);
                }
                if hr == DXGI_ERROR_ACCESS_LOST {
                    return Err(DxgiProbeError::AccessLost(hr));
                }
                if hr < 0 {
                    return Err(DxgiProbeError::Hresult(hr));
                }

                let _frame_guard = FrameGuard {
                    duplication: self.duplication.0,
                };
                let resource = ComPtr::new(resource);

                if !map_pointer_only && large_integer_value(&info.LastPresentTime) == 0 {
                    return Err(DxgiProbeError::PointerOnly(info));
                }

                let texture = query_texture2d(resource.0)?;
                let mut texture_desc: D3D11_TEXTURE2D_DESC = mem::zeroed();
                (*texture.0).GetDesc(&mut texture_desc);
                texture_desc.Usage = D3D11_USAGE_STAGING;
                texture_desc.BindFlags = 0;
                texture_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
                texture_desc.MiscFlags = 0;

                let mut readable = ptr::null_mut();
                let hr =
                    (*self.device.0).CreateTexture2D(&texture_desc, ptr::null(), &mut readable);
                hresult_to_probe(hr)?;
                let readable = ComPtr::new(readable);
                (*readable.0).SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM);

                (*self.context.0).CopyResource(
                    readable.0 as *mut ID3D11Resource,
                    texture.0 as *mut ID3D11Resource,
                );

                let mut mapped: D3D11_MAPPED_SUBRESOURCE = mem::zeroed();
                let hr = (*self.context.0).Map(
                    readable.0 as *mut ID3D11Resource,
                    0,
                    D3D11_MAP_READ,
                    0,
                    &mut mapped,
                );
                hresult_to_probe(hr)?;
                let _map_guard = MapGuard {
                    context: self.context.0,
                    resource: readable.0 as *mut ID3D11Resource,
                };

                let row_pitch = mapped.RowPitch;
                let len = row_pitch as usize * self.height as usize;
                if mapped.pData.is_null() || len == 0 {
                    return Err(DxgiProbeError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mapped DXGI frame has no data",
                    )));
                }
                let mapped_data = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len);
                let hash = hash_bgra_rows(mapped_data, self.width, self.height, row_pitch);
                let data = mapped_data.to_vec();
                Ok(CapturedFrame {
                    info,
                    width: self.width,
                    height: self.height,
                    row_pitch,
                    hash,
                    data,
                })
            }
        }
    }

    struct FrameGuard {
        duplication: *mut IDXGIOutputDuplication,
    }

    impl Drop for FrameGuard {
        fn drop(&mut self) {
            unsafe {
                // Safety: `duplication` is a live COM object owned by `DxgiCapturer`.
                // `FrameGuard` is created only after a successful AcquireNextFrame call.
                (*self.duplication).ReleaseFrame();
            }
        }
    }

    struct MapGuard {
        context: *mut ID3D11DeviceContext,
        resource: *mut ID3D11Resource,
    }

    impl Drop for MapGuard {
        fn drop(&mut self) {
            unsafe {
                // Safety: `resource` was successfully mapped on this immediate context.
                (*self.context).Unmap(self.resource, 0);
            }
        }
    }

    struct ComPtr<T>(*mut T);

    impl<T> ComPtr<T> {
        fn new(ptr: *mut T) -> Self {
            Self(ptr)
        }
    }

    impl<T> Drop for ComPtr<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    // Safety: COM interface pointers used here are acquired from Win32 calls
                    // that return an owned reference. Casting to IUnknown is valid for COM.
                    (*(self.0 as *mut IUnknown)).Release();
                }
            }
        }
    }

    unsafe fn create_factory() -> io::Result<ComPtr<IDXGIFactory1>> {
        let mut factory = ptr::null_mut();
        let hr = unsafe { CreateDXGIFactory1(&IDXGIFactory1::uuidof(), &mut factory) };
        hresult_to_io(hr, "CreateDXGIFactory1")?;
        Ok(ComPtr::new(factory.cast::<IDXGIFactory1>()))
    }

    unsafe fn get_adapter(
        factory: *mut IDXGIFactory1,
        index: UINT,
    ) -> io::Result<ComPtr<IDXGIAdapter1>> {
        let mut adapter = ptr::null_mut();
        let hr = unsafe { (*factory).EnumAdapters1(index, &mut adapter) };
        hresult_to_io(hr, "EnumAdapters1")?;
        Ok(ComPtr::new(adapter))
    }

    unsafe fn get_output(
        adapter: *mut IDXGIAdapter1,
        index: UINT,
    ) -> io::Result<ComPtr<IDXGIOutput>> {
        let mut output = ptr::null_mut();
        let hr = unsafe { (*adapter).EnumOutputs(index, &mut output) };
        hresult_to_io(hr, "EnumOutputs")?;
        Ok(ComPtr::new(output))
    }

    unsafe fn query_output1(output: *mut IDXGIOutput) -> io::Result<ComPtr<IDXGIOutput1>> {
        let mut output1 = ptr::null_mut();
        let hr = unsafe {
            (*output).QueryInterface(
                &IDXGIOutput1::uuidof(),
                &mut output1 as *mut *mut _ as *mut *mut _,
            )
        };
        hresult_to_io(hr, "IDXGIOutput1::QueryInterface")?;
        Ok(ComPtr::new(output1))
    }

    unsafe fn query_texture2d(
        resource: *mut IDXGIResource,
    ) -> Result<ComPtr<ID3D11Texture2D>, DxgiProbeError> {
        let mut texture = ptr::null_mut();
        let hr = unsafe {
            (*resource).QueryInterface(
                &ID3D11Texture2D::uuidof(),
                &mut texture as *mut *mut _ as *mut *mut _,
            )
        };
        hresult_to_probe(hr)?;
        Ok(ComPtr::new(texture))
    }

    fn enumerate_displays() -> io::Result<Vec<DisplayInfo>> {
        let mut displays = Vec::new();
        unsafe {
            let factory = create_factory()?;
            let mut adapter_index = 0;
            loop {
                let mut adapter = ptr::null_mut();
                let hr = (*factory.0).EnumAdapters1(adapter_index, &mut adapter);
                if hr == DXGI_ERROR_NOT_FOUND {
                    break;
                }
                hresult_to_io(hr, "EnumAdapters1")?;
                let adapter = ComPtr::new(adapter);
                let mut adapter_desc: DXGI_ADAPTER_DESC1 = mem::zeroed();
                hresult_to_io((*adapter.0).GetDesc1(&mut adapter_desc), "GetDesc1")?;
                let adapter_name = wide_to_string(&adapter_desc.Description);

                let mut output_index = 0;
                loop {
                    let mut output = ptr::null_mut();
                    let hr = (*adapter.0).EnumOutputs(output_index, &mut output);
                    if hr == DXGI_ERROR_NOT_FOUND {
                        break;
                    }
                    hresult_to_io(hr, "EnumOutputs")?;
                    let output = ComPtr::new(output);
                    let mut output_desc: DXGI_OUTPUT_DESC = mem::zeroed();
                    hresult_to_io((*output.0).GetDesc(&mut output_desc), "GetDesc")?;
                    displays.push(DisplayInfo {
                        adapter_index,
                        output_index,
                        adapter_name: adapter_name.clone(),
                        output_name: wide_to_string(&output_desc.DeviceName),
                        rect: output_desc.DesktopCoordinates,
                        attached_to_desktop: output_desc.AttachedToDesktop != 0,
                    });
                    output_index += 1;
                }
                adapter_index += 1;
            }
        }
        Ok(displays)
    }

    struct GdiSnapshot {
        width: u32,
        height: u32,
        stride: u32,
        hash: u64,
        data: Vec<u8>,
    }

    fn capture_gdi_snapshot(display: &DisplayInfo) -> io::Result<GdiSnapshot> {
        let width = display.width();
        let height = display.height();
        if width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty display rect",
            ));
        }
        unsafe {
            let screen_dc = GetDC(ptr::null_mut());
            if screen_dc.is_null() {
                return Err(last_os_error("GetDC"));
            }
            let _screen_guard = DcReleaseGuard {
                hwnd: ptr::null_mut(),
                dc: screen_dc,
            };

            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_null() {
                return Err(last_os_error("CreateCompatibleDC"));
            }
            let mem_guard = DcDeleteGuard { dc: mem_dc };

            let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
            if bitmap.is_null() {
                return Err(last_os_error("CreateCompatibleBitmap"));
            }
            let bitmap_guard = ObjectDeleteGuard {
                object: bitmap as HGDIOBJ,
            };

            let old_object = SelectObject(mem_guard.dc, bitmap_guard.object);
            if old_object.is_null() {
                return Err(last_os_error("SelectObject"));
            }
            let _select_guard = SelectObjectGuard {
                dc: mem_guard.dc,
                old_object,
            };

            let ok = BitBlt(
                mem_guard.dc,
                0,
                0,
                width as i32,
                height as i32,
                screen_dc,
                display.rect.left,
                display.rect.top,
                SRCCOPY | CAPTUREBLT,
            );
            if ok == FALSE {
                return Err(last_os_error("BitBlt"));
            }

            let stride = width * 4;
            let mut data = vec![0_u8; stride as usize * height as usize];
            let mut info: BITMAPINFO = mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as DWORD,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: data.len() as DWORD,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let lines = GetDIBits(
                mem_guard.dc,
                bitmap as HBITMAP,
                0,
                height,
                data.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );
            if lines == 0 {
                return Err(last_os_error("GetDIBits"));
            }
            let hash = hash_bgra_rows(&data, width, height, stride);
            Ok(GdiSnapshot {
                width,
                height,
                stride,
                hash,
                data,
            })
        }
    }

    struct DcReleaseGuard {
        hwnd: winapi::shared::windef::HWND,
        dc: HDC,
    }

    impl Drop for DcReleaseGuard {
        fn drop(&mut self) {
            if !self.dc.is_null() {
                unsafe {
                    // Safety: DC was acquired by GetDC for this HWND.
                    ReleaseDC(self.hwnd, self.dc);
                }
            }
        }
    }

    struct DcDeleteGuard {
        dc: HDC,
    }

    impl Drop for DcDeleteGuard {
        fn drop(&mut self) {
            unsafe {
                // Safety: DC was created by CreateCompatibleDC.
                DeleteDC(self.dc);
            }
        }
    }

    struct ObjectDeleteGuard {
        object: HGDIOBJ,
    }

    impl Drop for ObjectDeleteGuard {
        fn drop(&mut self) {
            unsafe {
                // Safety: object was created by CreateCompatibleBitmap.
                DeleteObject(self.object);
            }
        }
    }

    struct SelectObjectGuard {
        dc: HDC,
        old_object: HGDIOBJ,
    }

    impl Drop for SelectObjectGuard {
        fn drop(&mut self) {
            unsafe {
                // Safety: restores the previous object selected into this memory DC.
                SelectObject(self.dc, self.old_object);
            }
        }
    }

    struct WakeWindow {
        hwnd: winapi::shared::windef::HWND,
        visible: bool,
    }

    impl WakeWindow {
        fn new() -> io::Result<Self> {
            unsafe {
                let class_name = wide_null("RustAdminDxgiProbeWakeWindow");
                let hinstance = GetModuleHandleW(ptr::null());
                let wc = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(DefWindowProcW),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinstance,
                    hIcon: ptr::null_mut(),
                    hCursor: ptr::null_mut(),
                    hbrBackground: ptr::null_mut(),
                    lpszMenuName: ptr::null(),
                    lpszClassName: class_name.as_ptr(),
                };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST
                        | WS_EX_NOACTIVATE,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    hinstance,
                    ptr::null_mut(),
                );
                if hwnd.is_null() {
                    return Err(last_os_error("CreateWindowExW"));
                }
                SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    1,
                    1,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
                Ok(Self {
                    hwnd,
                    visible: false,
                })
            }
        }

        fn pulse(&mut self) {
            unsafe {
                if self.visible {
                    ShowWindow(self.hwnd, SW_HIDE);
                } else {
                    ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                    UpdateWindow(self.hwnd);
                }
            }
            self.visible = !self.visible;
        }
    }

    impl Drop for WakeWindow {
        fn drop(&mut self) {
            unsafe {
                // Safety: hwnd was created by this process.
                DestroyWindow(self.hwnd);
            }
        }
    }

    fn apply_wake(wake: WakeMode) {
        unsafe {
            match wake {
                WakeMode::None | WakeMode::LayeredWindow => {}
                WakeMode::Cursor => {
                    let mut point: POINT = mem::zeroed();
                    if GetCursorPos(&mut point) != FALSE {
                        SetCursorPos(point.x + 1, point.y);
                        thread::sleep(Duration::from_millis(10));
                        SetCursorPos(point.x, point.y);
                    }
                }
                WakeMode::Redraw => {
                    RedrawWindow(
                        GetDesktopWindow(),
                        ptr::null::<RECT>(),
                        ptr::null_mut(),
                        RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
                    );
                }
            }
        }
    }

    struct JsonLogger {
        writer: BufWriter<File>,
        seq: u64,
    }

    impl JsonLogger {
        fn new(path: &Path) -> io::Result<Self> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Ok(Self {
                writer: BufWriter::new(File::create(path)?),
                seq: 0,
            })
        }

        fn event(&mut self, event: &str, fields: String) -> io::Result<()> {
            self.seq += 1;
            let ts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            if fields.is_empty() {
                writeln!(
                    self.writer,
                    "{{\"ts_ms\":{ts_ms},\"seq\":{},\"event\":\"{}\"}}",
                    self.seq,
                    escape_json(event)
                )
            } else {
                writeln!(
                    self.writer,
                    "{{\"ts_ms\":{ts_ms},\"seq\":{},\"event\":\"{}\",{fields}}}",
                    self.seq,
                    escape_json(event)
                )
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }

    fn save_bgra_bmp(
        path: &Path,
        width: u32,
        height: u32,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let row_bytes = width as usize * 4;
        let stride = stride as usize;
        if data.len() < stride * height as usize || stride < row_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame buffer is smaller than BMP dimensions",
            ));
        }

        let pixel_bytes = row_bytes * height as usize;
        let file_size = 14 + 40 + pixel_bytes;
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(b"BM")?;
        writer.write_all(&(file_size as u32).to_le_bytes())?;
        writer.write_all(&[0; 4])?;
        writer.write_all(&(54_u32).to_le_bytes())?;
        writer.write_all(&(40_u32).to_le_bytes())?;
        writer.write_all(&(width as i32).to_le_bytes())?;
        writer.write_all(&(height as i32).to_le_bytes())?;
        writer.write_all(&(1_u16).to_le_bytes())?;
        writer.write_all(&(32_u16).to_le_bytes())?;
        writer.write_all(&(0_u32).to_le_bytes())?;
        writer.write_all(&(pixel_bytes as u32).to_le_bytes())?;
        writer.write_all(&(2835_i32).to_le_bytes())?;
        writer.write_all(&(2835_i32).to_le_bytes())?;
        writer.write_all(&(0_u32).to_le_bytes())?;
        writer.write_all(&(0_u32).to_le_bytes())?;

        for row in (0..height as usize).rev() {
            let start = row * stride;
            writer.write_all(&data[start..start + row_bytes])?;
        }
        writer.flush()
    }

    fn hash_bgra_rows(data: &[u8], width: u32, height: u32, stride: u32) -> u64 {
        let row_bytes = width as usize * 4;
        let stride = stride as usize;
        let mut hash = FNV_OFFSET;
        for row in 0..height as usize {
            let start = row * stride;
            let end = start + row_bytes;
            if end > data.len() {
                break;
            }
            for byte in &data[start..end] {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        hash
    }

    fn hresult_to_io(hr: HRESULT, context: &str) -> io::Result<()> {
        if hr >= 0 {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{context} failed: 0x{:08x}", hr as u32),
        ))
    }

    fn hresult_to_probe(hr: HRESULT) -> Result<(), DxgiProbeError> {
        if hr >= 0 {
            return Ok(());
        }
        if hr == DXGI_ERROR_ACCESS_LOST {
            return Err(DxgiProbeError::AccessLost(hr));
        }
        Err(DxgiProbeError::Hresult(hr))
    }

    fn last_os_error(context: &str) -> io::Error {
        let err = io::Error::last_os_error();
        io::Error::new(err.kind(), format!("{context} failed: {err}"))
    }

    fn large_integer_value(value: &winapi::shared::ntdef::LARGE_INTEGER) -> i64 {
        unsafe {
            // Safety: reading the QuadPart view of LARGE_INTEGER is how winapi exposes it.
            *value.QuadPart()
        }
    }

    fn wide_to_string(wide: &[u16]) -> String {
        let len = wide.iter().position(|ch| *ch == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..len])
    }

    fn wide_null(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn json_error(err: &io::Error) -> String {
        format!("\"error\":\"{}\"", escape_json(&err.to_string()))
    }

    fn escape_json(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
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
        out
    }
}
