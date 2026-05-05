# Bugs Review - RustDesk / RustAdmin Fork - 2026-05-06

## Scope

Bug/quality review for the same local checkout used in `security_review_20260506.md`: `rustdesk`, `rustadmin-server`, `hbb_common`, `hwcodec`, and `rustadmin-tests`. Items below are not limited to exploitable security bugs; several are correctness, testability, or hardening bugs that can block safe operation.

## Findings

### BUG-001 - High - Safety and fuzz harnesses point to a nonexistent `hbb_common`

The safety probes and fuzz crate do not build in this checkout:

- `rustadmin-tests/dynamic/safety_probes/Cargo.toml:8`
- `rustadmin-tests/fuzz/Cargo.toml:12`

Both reference `rustdesk-client/libs/hbb_common`, which does not exist in this repo layout. Current commands fail before building:

```text
failed to read /Users/s02299/GH/rustdesk/rustdesk-client/libs/hbb_common/Cargo.toml
```

**Impact:** the hardening probes that should catch regressions in codec allocation, zstd expansion, IPC permissions, and file-transfer path handling are currently dead. This is a high-priority project bug because it creates false confidence during hardening.

**Fix:** update paths to the top-level `hbb_common`:

- dynamic probes: `../../../hbb_common`
- fuzz crate: `../../hbb_common`

Then run probes with `EXPECT_HARDENED=1` in CI.

### BUG-002 - Medium-High - Online request cap bug allocates from untrusted list length

Same root cause as `SEC-003`.

`rustadmin-server/src/rendezvous_server.rs:996-1028` caps the lookup loop but allocates `states` from `peers.len()`. This contradicts `MAX_ONLINE_REQUEST_PEERS` and can make response size larger than the configured cap.

**Fix:** reject or truncate before allocation and test the response byte length.

### BUG-003 - Medium - `always-use-relay` admin command also reparses `Y`/`N` as relay server config

`rustadmin-server/src/rendezvous_server.rs:1272-1279` toggles `ALWAYS_USE_RELAY`, then sends `Data::RelayServers0(rs.to_owned())` using the same argument. For commands like `aur Y` or `aur N`, the value `Y`/`N` can be fed to the relay-server parser as if it were a relay server list.

**Impact:** a local admin command intended to toggle one setting can unexpectedly corrupt or clear runtime relay-server configuration.

**Fix:** remove the `Data::RelayServers0` send from `always-use-relay`, or split the command into separate arguments with explicit parsing.

### BUG-004 - Medium - Decompression errors are silently converted into empty payloads

`hbb_common/src/compress.rs:32-33` uses `zstd::decode_all(data).unwrap_or_default()`.

**Impact:** callers cannot tell the difference between a valid empty payload and malformed compressed data. File transfer can silently write empty decompressed blocks; clipboard and terminal paths can silently drop data. This also masks security telemetry that would help detect malformed peer traffic.

**Fix:** return `Result<Vec<u8>>` from decompression and propagate a protocol/file-transfer error to the caller.

### BUG-005 - Medium-Low - File-transfer write path has a known TOCTOU symlink race

`hbb_common/src/fs.rs:779-783` explicitly documents that the path-based validation plus regular open still has a symlink race. The file is then opened with `File::create(&path)` at line 798.

**Impact:** remote path traversal and obvious symlink-component cases appear guarded, but a local attacker who can race the destination path may still redirect a write. This is lower priority than remote-only bugs, but it matters for hardened multi-user hosts.

**Fix:** use descriptor/handle-based no-follow creation: `openat`/`O_NOFOLLOW` on Unix and reparse-point-safe `CreateFile` flags on Windows. Keep the existing path validation as an outer check.

### BUG-006 - Medium-Low - FFmpeg RAM decode callback does unchecked size math

`hwcodec/src/ffmpeg_ram/decode.rs:130-146` converts `linesize * height` to `usize` and builds slices without checking null pointers, negative dimensions, overflow, or maximum frame bytes.

**Impact:** malformed decoder callback metadata can cause panic/OOM/undefined behavior in the client decode wrapper.

**Fix:** move callback validation into a small safe helper that checks dimensions, pointer presence, checked multiplication, and max frame size before copying bytes.

### BUG-007 - Medium-Low - Server private key file creation relies on umask

`rustadmin-server/src/common.rs:417-420` creates `id_ed25519` with plain `File::create`.

**Impact:** file mode depends on deployment umask and can be too open on Unix. This is both a security finding and an operational footgun.

**Fix:** create with `create_new` and `0o600`, and validate existing key permissions at startup.

### BUG-008 - Medium-Low - Dependency security tooling is missing

`cargo audit --version` and `cargo deny --version` both fail because the subcommands are not installed. No `deny.toml`/`cargo-deny.toml` was found in the checkout.

**Impact:** stale or vulnerable dependencies can land without a local or CI gate. This is visible now in the server WebSocket stack.

**Fix:** add `cargo audit` and `cargo deny` to the hardening workflow, commit a policy file, and run both against `rustadmin-server`, `rustdesk`, `hbb_common`, and test/fuzz crates.

### BUG-009 - Low - WebSocket dependency versions are split between old server direct deps and newer shared deps

`rustadmin-server` directly depends on `tokio-tungstenite 0.17`/`tungstenite 0.17`, while `hbb_common` uses the newer `0.26` line. The server lock therefore contains both old and new WebSocket stacks.

**Impact:** larger dependency surface and a real old-version security bug. Even after the security fix, keeping duplicate major/minor stacks increases maintenance risk.

**Fix:** align RustAdmin server WebSocket usage with the `hbb_common` `0.26` line unless a concrete compatibility blocker exists.

## Fixed / Not Reproduced From Earlier Baseline

- `hbb_common/src/bytes_codec.rs` now rejects packets above the default max length and does not preallocate from header-only packets.
- IPC path hardening now rejects symlink IPC paths and uses restrictive Unix permissions.
- Server/client lockfiles are currently accepted by `cargo metadata --locked`.
- Basic file-transfer traversal and symlink-component rejection appears to be present, with the TOCTOU race tracked separately above.

## Verification Performed

- `cargo metadata --locked` for `rustadmin-server`, `rustdesk`, and `hbb_common`
- attempted dynamic safety probes; blocked by broken manifest paths
- checked for `cargo audit`, `cargo deny`, and `deny.toml`
- targeted source review of bug-prone paths: compression, file transfer, IPC, rendezvous/relay commands, WebSocket deps, and `hwcodec` callbacks
