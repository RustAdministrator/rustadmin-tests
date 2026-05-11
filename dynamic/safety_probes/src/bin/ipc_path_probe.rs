#[cfg(not(windows))]
use hbb_common::config::is_service_ipc_postfix;
use hbb_common::config::{Config, APP_NAME};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::SystemTime,
};

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

fn parent_of(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_default()
}

fn normal_case() -> bool {
    let app_name = unique_name("RustDeskSafetyIpcNormal");
    #[cfg(unix)]
    let saved_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    #[cfg(unix)]
    let runtime_root = {
        let runtime_root = std::env::temp_dir().join(unique_name("rustdesk_ipc_normal_runtime"));
        let _ = std::fs::remove_dir_all(&runtime_root);
        std::fs::create_dir_all(&runtime_root).expect("create normal runtime root");
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_root);
        runtime_root
    };

    set_app_name(&app_name);
    let ipc_path = Config::ipc_path("_probe");
    let dir = parent_of(&ipc_path);
    println!("case=normal");
    println!("ipc_path={ipc_path}");
    println!("dir={}", dir.display());
    let dir_mode = mode(&dir);
    #[cfg(target_os = "linux")]
    let current_uid_matches = {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(&runtime_root)
            .expect("normal runtime root metadata")
            .uid();
        Config::ipc_path_for_uid(uid, "_probe") == ipc_path
    };
    #[cfg(not(target_os = "linux"))]
    let current_uid_matches = true;
    let reproduced = !current_uid_matches || dir_mode.map(|mode| mode & 0o022 != 0).unwrap_or(true);
    println!("dir_mode={dir_mode:?}");
    println!("current_uid_matches={current_uid_matches}");
    println!("finding_reproduced={reproduced}");
    let _ = std::fs::remove_dir_all(&dir);
    #[cfg(unix)]
    {
        match saved_runtime_dir {
            Some(path) => std::env::set_var("XDG_RUNTIME_DIR", path),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        let _ = std::fs::remove_dir_all(&runtime_root);
    }
    reproduced
}

#[cfg(unix)]
fn service_case() -> bool {
    let app_name = unique_name("RustDeskSafetyIpcService");
    set_app_name(&app_name);

    let service_path = Config::ipc_path("_service");
    let uinput_path = Config::ipc_path("_uinput_keyboard");
    let service_dir = parent_of(&service_path);
    let uinput_dir = parent_of(&uinput_path);
    let service_mode = mode(&service_dir);
    let uinput_mode = mode(&uinput_dir);

    #[cfg(target_os = "linux")]
    let cross_uid_shared = {
        Config::ipc_path_for_uid(1000, "_service") == Config::ipc_path_for_uid(2000, "_service")
            && Config::ipc_path_for_uid(1000, "_uinput_keyboard")
                == Config::ipc_path_for_uid(2000, "_uinput_keyboard")
            && Config::ipc_path_for_uid(1000, "_probe") != Config::ipc_path_for_uid(2000, "_probe")
    };
    #[cfg(not(target_os = "linux"))]
    let cross_uid_shared = true;

    let reproduced = !is_service_ipc_postfix("_service")
        || !is_service_ipc_postfix("_uinput_keyboard")
        || is_service_ipc_postfix("_probe")
        || service_dir != uinput_dir
        || !cross_uid_shared
        || service_mode.map(|mode| mode & 0o022 != 0).unwrap_or(true)
        || uinput_mode.map(|mode| mode & 0o022 != 0).unwrap_or(true);

    println!("case=service");
    println!("service_path={service_path}");
    println!("uinput_path={uinput_path}");
    println!("service_dir={}", service_dir.display());
    println!("uinput_dir={}", uinput_dir.display());
    println!("service_mode={service_mode:?}");
    println!("uinput_mode={uinput_mode:?}");
    println!("cross_uid_shared={cross_uid_shared}");
    println!("finding_reproduced={reproduced}");

    let _ = std::fs::remove_dir_all(&service_dir);
    let _ = std::fs::remove_dir_all(&uinput_dir);

    reproduced
}

#[cfg(not(unix))]
fn service_case() -> bool {
    println!("case=service skipped on non-Unix platform");
    false
}

#[cfg(unix)]
fn symlink_case() -> bool {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    let app_name = unique_name("RustDeskSafetyIpcSymlink");
    let saved_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let runtime_root = std::env::temp_dir().join(unique_name("rustdesk_ipc_runtime"));
    let _ = std::fs::remove_dir_all(&runtime_root);
    std::fs::create_dir_all(&runtime_root).expect("create runtime root");
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_root);

    let uid = std::fs::metadata(&runtime_root)
        .expect("runtime root metadata")
        .uid();
    let link_path = runtime_root.join(format!("{app_name}-{uid}"));
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
    let reproduced = target_mode != Some(0o700);

    println!("case=symlink");
    println!("ipc_path={ipc_path}");
    println!("link_path={}", link_path.display());
    println!("target_path={}", target_path.display());
    println!("target_mode={target_mode:?}");
    println!("finding_reproduced={reproduced}");

    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_dir_all(&target_path);
    let _ = std::fs::remove_dir_all(&runtime_root);
    match saved_runtime_dir {
        Some(path) => std::env::set_var("XDG_RUNTIME_DIR", path),
        None => std::env::remove_var("XDG_RUNTIME_DIR"),
    }

    reproduced
}

#[cfg(not(unix))]
fn symlink_case() -> bool {
    println!("case=symlink skipped on non-Unix platform");
    false
}

fn main() -> ExitCode {
    let normal_reproduced = normal_case();
    let service_reproduced = service_case();
    let symlink_reproduced = symlink_case();
    let reproduced = normal_reproduced || service_reproduced || symlink_reproduced;
    if reproduced && std::env::var("EXPECT_HARDENED").ok().as_deref() == Some("1") {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
