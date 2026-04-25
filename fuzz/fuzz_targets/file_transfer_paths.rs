#![no_main]

use hbb_common::{
    fs::{DataSource, JobType, TransferJob},
    message_proto::FileEntry,
};
use libfuzzer_sys::fuzz_target;

fn file_entry(name: String) -> FileEntry {
    let mut entry = FileEntry::new();
    entry.name = name;
    entry
}

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data).into_owned();
    let base = std::env::temp_dir().join("rustdesk_safety_fuzz_file_transfer_paths");
    let mut job = TransferJob::new_write(
        1,
        JobType::Generic,
        "/remote".to_owned(),
        DataSource::FilePath(base),
        0,
        false,
        true,
        false,
    );
    let _ = job.set_files(vec![file_entry(name)]);
});
