# RustDesk Safety Tests

This directory contains defensive safety tests for this RustDesk fork. It is intentionally outside `rustdesk-client` and `rustdesk-server` so the original source trees are not changed.

The tests are split into:
- advisory and dependency checks
- cargo-fuzz harnesses for parser/codec/file-transfer surfaces
- localhost-only dynamic probes
- resource checks for allocation and decompression behavior

## Existing Fuzzing Status

No existing RustDesk `cargo-fuzz`, AFL, honggfuzz, or `fuzz_targets` suite was found in `rustdesk-client` or `rustdesk-server` during setup. There are normal Rust tests in `hbb_common`, including file-transfer path validation tests in `rustdesk-client/libs/hbb_common/src/fs.rs`.

## Quick Start

Run local advisory/dependency checks that do not require missing tools:

```bash
./rustdesk-tests/scripts/run_advisories.sh
```

Run dynamic localhost/resource probes:

```bash
./rustdesk-tests/scripts/run_dynamic_local.sh
./rustdesk-tests/scripts/run_resource_checks.sh
```

Run short fuzz smoke tests:

```bash
./rustdesk-tests/scripts/run_fuzz_smoke.sh 60
```

The runner sets `ASAN_OPTIONS=detect_leaks=0:detect_odr_violation=0` by default because LeakSanitizer can fail under sandbox/ptrace-style execution environments even when the fuzz target completes normally.

For online checks, install the missing tools and opt in:

```bash
RUN_ONLINE=1 ./rustdesk-tests/scripts/run_advisories.sh
```

Useful tools:
- `cargo-audit`
- `cargo-deny`
- `osv-scanner`
- `cargo-fuzz`
- `npm`
- `flutter`

## Dynamic Probes

The dynamic probes are local-only. They do not scan public targets.

Available binaries:

```bash
cargo run --manifest-path rustdesk-tests/dynamic/safety_probes/Cargo.toml --bin ipc_path_probe
cargo run --manifest-path rustdesk-tests/dynamic/safety_probes/Cargo.toml --bin codec_header_alloc_probe
cargo run --manifest-path rustdesk-tests/dynamic/safety_probes/Cargo.toml --bin file_transfer_path_probe
cargo run --manifest-path rustdesk-tests/dynamic/safety_probes/Cargo.toml --bin zstd_expansion_probe
cargo run --manifest-path rustdesk-tests/dynamic/safety_probes/Cargo.toml --bin online_request_client -- 127.0.0.1:21115 10000
```

Set `EXPECT_HARDENED=1` to make probes return non-zero when a known unsafe behavior is reproduced. Without that variable, probes report findings but exit successfully so they can be used for baseline collection.

## Fuzz Targets

Cargo-fuzz targets live in `fuzz/fuzz_targets/`.

Targets:
- `bytes_codec_decode`
- `rendezvous_message_decode`
- `message_decode`
- `file_transfer_paths`
- `zstd_decompress`

Examples:

```bash
cd rustdesk-tests
cargo fuzz run bytes_codec_decode -- -max_total_time=60
cargo fuzz run rendezvous_message_decode -- -max_total_time=60
```

The `bytes_codec_decode` fuzz target caps packet length to keep fuzzing safe. Use `codec_header_alloc_probe` for the specific default-header allocation regression check.

## Result Files

Runner scripts write logs under:

```text
rustdesk-tests/results/YYYYMMDD-HHMMSS/
```

The directory includes raw logs and a `summary.md` suitable for hardening notes.
