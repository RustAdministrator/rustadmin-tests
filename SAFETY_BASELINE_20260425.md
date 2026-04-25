# Safety Baseline - 2026-04-25

This baseline was produced from the standalone `rustdesk-tests` harness. No original RustDesk source files were intentionally changed. A generated `rustdesk-server/Cargo.lock` change from Cargo metadata resolution was reverted.

## Fuzzing Status

No pre-existing RustDesk cargo-fuzz/AFL/honggfuzz suite was found in `rustdesk-client` or `rustdesk-server`.

New cargo-fuzz targets added:
- `bytes_codec_decode`
- `rendezvous_message_decode`
- `message_decode`
- `file_transfer_paths`
- `zstd_decompress`

Smoke run:
- Results directory: `rustdesk-tests/results/20260425-014421/fuzz`
- Duration: 5 seconds per target
- Toolchain: nightly via `cargo +nightly fuzz`
- LeakSanitizer disabled in the runner with `ASAN_OPTIONS=detect_leaks=0:detect_odr_violation=0`
- Result: all five targets exited 0; no crash found in this short smoke run.

## Dynamic Local Baseline

Results directory: `rustdesk-tests/results/20260425-011415/dynamic`

Findings reproduced:
- `ipc_path_probe`: normal IPC directory mode is `0777`; symlink target mode became `0777`.
- `codec_header_alloc_probe`: a 64 MiB advertised packet length changed `BytesMut` capacity from `4` to `67108864` before body bytes arrived.
- `zstd_expansion_probe`: 8 MiB of zero bytes compressed to 275 bytes and decompressed back to 8 MiB, expansion ratio `30504.03`.

Passing hardening checks:
- `file_transfer_path_probe`: relative traversal, absolute path, null byte, and symlink-component cases were rejected; a safe relative path was accepted.

Skipped:
- `online_request_client` was skipped because `HBBS_ADDR` was not set.

## Resource Baseline

Results directory: `rustdesk-tests/results/20260425-014954/resource`

Measured probe-only runs:
- `codec_header_alloc_probe`: reproduced capacity growth to `67108864`; maximum RSS in `/usr/bin/time` was low because Linux overcommit does not fault every reserved page.
- `zstd_expansion_probe`: decompressed 275 bytes to 8 MiB; maximum RSS was `13824` KiB in this run.

## Advisory Baseline

Final online results directory: `rustdesk-tests/results/20260425-202850/advisories`

Completed:
- `rustdesk-client` locked `cargo tree -d`: exit 0
- `rustdesk-client` locked `cargo metadata`: exit 0
- `rustdesk-client` `cargo audit`: exit 1
- `rustdesk-client` `cargo deny --locked check`: exit 5
- `rustdesk-server` `cargo audit`: exit 1
- `rustdesk-client/flutter` `flutter pub outdated`: exit 0

Locked dependency integrity issue:
- `rustdesk-server` locked `cargo tree -d`: exit 101.
- `rustdesk-server` locked `cargo metadata`: exit 101.
- `rustdesk-server` `cargo deny --locked check`: exit 1.
- Cause: Cargo reports that `rustdesk-server/Cargo.lock` would need to change, but `--locked` prevents mutation. The runner now keeps this as a finding instead of rewriting the original lockfile.

RustSec highlights:
- Client: `cargo audit` reported 16 vulnerabilities and 39 allowed warnings.
- Server: `cargo audit` reported 18 vulnerabilities and 21 allowed warnings.
- High-priority client advisories include `bytes` `RUSTSEC-2026-0007`, `libgit2-sys` `RUSTSEC-2024-0013`, `quinn-proto` `RUSTSEC-2026-0037`, `openssl` `RUSTSEC-2025-0022`/`RUSTSEC-2025-0004`, `rustls-webpki` `RUSTSEC-2026-0098`/`0099`/`0104`, and `users` `RUSTSEC-2025-0040`.
- High-priority server advisories include `axum-core` `RUSTSEC-2022-0055`, `libsqlite3-sys` `RUSTSEC-2022-0090`, `openssl` `RUSTSEC-2025-0022`/`RUSTSEC-2025-0004`, `protobuf` `RUSTSEC-2024-0437`, `rustls` `RUSTSEC-2024-0336`, `rustls-webpki` `RUSTSEC-2026-0098`/`0099`/`0104`, `sqlx` `RUSTSEC-2024-0363`, `tungstenite` `RUSTSEC-2023-0065`, and `webpki` `RUSTSEC-2023-0052`.

Supply-chain and online notes:
- `cargo-deny` is currently running with its default policy because no `deny.toml` was found. It reports advisory failures and license failures for the client. For production CI, add an explicit `deny.toml` with your accepted licenses, source allowlist, and advisory exceptions.
- `osv-scanner` was skipped because it is not installed.
- Server UI `npm audit` was skipped because `rustdesk-server/ui/html` has no `package-lock.json` or `npm-shrinkwrap.json`.
- Flutter outdated check found 3 packages locked to older versions, 40 dependencies constrained below a resolvable newer version, and discontinued packages `js`, `build_resolvers`, and `build_runner_core`.

Recommended hardening order:
1. Fix server lockfile reproducibility first so `cargo metadata --locked` works without mutation.
2. Upgrade direct security-sensitive crates before chasing broad unmaintained warnings: `bytes`, TLS stack crates, `protobuf`, `sqlx`, `tungstenite`, `axum`, and `quinn`.
3. Add a project-owned `deny.toml` and make `cargo audit`, `cargo deny --locked check`, and locked metadata mandatory in CI.
4. Add a package lock for the server UI if npm dependencies are part of shipped builds; otherwise document why the UI dependency graph is intentionally unlocked.
5. Replace or isolate unmaintained crates that touch platform/user identity or crypto-adjacent code, especially `users` and old GTK/transitive desktop crates where applicable.

## Useful Commands

Run local dynamic probes:

```bash
./rustdesk-tests/scripts/run_dynamic_local.sh
```

Run resource probes:

```bash
./rustdesk-tests/scripts/run_resource_checks.sh
```

Run fuzz smoke:

```bash
./rustdesk-tests/scripts/run_fuzz_smoke.sh 60
```

Run the rendezvous online request probe against a local test server:

```bash
HBBS_ADDR=127.0.0.1:21115 ONLINE_REQUEST_PEERS=10000 ./rustdesk-tests/scripts/run_dynamic_local.sh
```

Make known unsafe behavior fail CI after hardening:

```bash
EXPECT_HARDENED=1 ./rustdesk-tests/scripts/run_dynamic_local.sh
EXPECT_HARDENED=1 ./rustdesk-tests/scripts/run_resource_checks.sh
```
