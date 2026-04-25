use hbb_common::config::{Config, APP_NAME};
use std::{path::PathBuf, process::ExitCode, time::SystemTime};

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

fn set_app_name(name: &str) {
    *APP_NAME.write().unwrap() = name.to_owned();
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode(_path: &std::path::Path) -> Option<u32> {
    None
}

fn normal_case() {
    let app_name = unique_name("RustDeskSafetyIpcNormal");
    set_app_name(&app_name);
    let ipc_path = Config::ipc_path("_probe");
    let dir = PathBuf::from(&ipc_path)
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_default();
    println!("case=normal");
    println!("ipc_path={ipc_path}");
    println!("dir={}", dir.display());
    println!("dir_mode={:?}", mode(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn symlink_case() -> bool {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let app_name = unique_name("RustDeskSafetyIpcSymlink");
    let link_path = PathBuf::from(format!("/tmp/{app_name}"));
    let target_path = std::env::temp_dir().join(unique_name("rustdesk_ipc_symlink_target"));
    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_dir_all(&target_path);
    std::fs::create_dir_all(&target_path).expect("create symlink target");
    std::fs::set_permissions(&target_path, std::fs::Permissions::from_mode(0o700))
        .expect("set target mode");
    symlink(&target_path, &link_path).expect("create app-name symlink");

    set_app_name(&app_name);
    let ipc_path = Config::ipc_path("_probe");
    let target_mode = mode(&target_path);
    let reproduced = target_mode == Some(0o777);

    println!("case=symlink");
    println!("ipc_path={ipc_path}");
    println!("link_path={}", link_path.display());
    println!("target_path={}", target_path.display());
    println!("target_mode={target_mode:?}");
    println!("finding_reproduced={reproduced}");

    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_dir_all(&target_path);

    reproduced
}

#[cfg(not(unix))]
fn symlink_case() -> bool {
    println!("case=symlink skipped on non-Unix platform");
    false
}

fn main() -> ExitCode {
    normal_case();
    let reproduced = symlink_case();
    if reproduced && std::env::var("EXPECT_HARDENED").ok().as_deref() == Some("1") {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
