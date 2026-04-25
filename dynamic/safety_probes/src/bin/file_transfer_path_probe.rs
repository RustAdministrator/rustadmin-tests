use hbb_common::{
    fs::{DataSource, JobType, TransferJob},
    message_proto::FileEntry,
};
use std::{path::PathBuf, process::ExitCode, time::SystemTime};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), nanos))
}

fn file_entry(name: &str) -> FileEntry {
    let mut entry = FileEntry::new();
    entry.name = name.to_owned();
    entry
}

fn new_job(base: PathBuf) -> TransferJob {
    TransferJob::new_write(
        1,
        JobType::Generic,
        "/remote".to_owned(),
        DataSource::FilePath(base),
        0,
        false,
        true,
        false,
    )
}

fn check_case(base: PathBuf, name: &str, expected_reject: bool) -> bool {
    let mut job = new_job(base);
    let rejected = job.set_files(vec![file_entry(name)]).is_err();
    let ok = rejected == expected_reject;
    println!(
        "case={:?} expected_reject={} rejected={} ok={}",
        name, expected_reject, rejected, ok
    );
    ok
}

fn main() -> ExitCode {
    let root = unique_temp_dir("rustdesk_file_transfer_probe");
    let downloads = root.join("downloads");
    let _ = std::fs::create_dir_all(&downloads);

    let mut all_ok = true;
    all_ok &= check_case(downloads.clone(), "../escape.txt", true);
    all_ok &= check_case(downloads.clone(), "/tmp/escape.txt", true);
    all_ok &= check_case(downloads.clone(), "safe/file.txt", false);
    all_ok &= check_case(downloads.clone(), "bad\0name.txt", true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = root.join("outside");
        let link = downloads.join("link");
        let _ = std::fs::create_dir_all(&outside);
        let _ = symlink(&outside, &link);
        all_ok &= check_case(downloads.clone(), "link/escape.txt", true);
    }

    let _ = std::fs::remove_dir_all(&root);

    if !all_ok && std::env::var("EXPECT_HARDENED").ok().as_deref() == Some("1") {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
