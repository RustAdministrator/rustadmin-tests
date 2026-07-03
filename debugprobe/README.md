# RustAdmin Debug Probe

Standalone Windows diagnostics for cases where RustAdmin's GUI reacts slowly but
we do not want to rebuild the full application.

Build from the repo root on Windows:

```powershell
cargo build --manifest-path .\rustdesk-tests\debugprobe\Cargo.toml --release
```

Run on the affected Windows machine:

```powershell
.\rustadmin-debugprobe.exe --duration 45 --interval-ms 1000
```

Optional dump capture, useful while the GUI is stuck:

```powershell
.\rustadmin-debugprobe.exe --duration 45 --dump
```

The probe writes a timestamped folder under the current user's Desktop by
default. It collects process snapshots, RustAdmin top-level window
responsiveness via `SendMessageTimeout`, named-pipe visibility, app/service log
copies, basic filesystem latency checks, command output snapshots, and optional
minidumps.

This external tool cannot inspect Flutter Rust Bridge worker queues inside
RustAdmin. It is meant to separate external system/service/log/file issues from
in-process bridge stalls and to capture enough state for stack inspection.
