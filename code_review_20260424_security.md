# RustDesk Security Review Notes - Safety Tests

Generated: 2026-04-25

Scope: standalone probes, fuzz harnesses, and dependency advisory checks in `rustdesk-tests`. The original `rustdesk-client`, `rustdesk-server`, and `hbb_common` source trees were not intentionally changed.

## Evidence

- Baseline report: `rustdesk-tests/SAFETY_BASELINE_20260425.md`
- Dynamic results: `rustdesk-tests/results/20260425-011415/dynamic`
- Fuzz smoke results: `rustdesk-tests/results/20260425-014421/fuzz`
- Resource results: `rustdesk-tests/results/20260425-014954/resource`
- Online advisory results: `rustdesk-tests/results/20260425-202850/advisories`

## Findings

### SEC-001: Untrusted frame length can force large pre-body allocation

`codec_header_alloc_probe` reproduced a 64 MiB advertised packet length growing `BytesMut` capacity from `4` to `67108864` before body bytes arrived.

Attack sides: client from server, server from client, internet-exposed relay/rendezvous surfaces, MITM that can inject or replay framed traffic before authentication.

Risk: memory exhaustion and allocator pressure with low bandwidth. This is especially relevant on LTE/VPN paths where an attacker can send many small headers slowly.

Recommendations:
- Enforce per-protocol and per-state maximum frame sizes before calling `reserve`.
- Keep unauthenticated caps much lower than authenticated/session caps.
- Add per-peer byte budgets, concurrent-frame budgets, and timeout-based cleanup.
- Make the test fail in CI with `EXPECT_HARDENED=1` after the cap is implemented.

### SEC-002: IPC directory chmod follows symlink target behavior

`ipc_path_probe` reproduced normal IPC directory mode `0777` and symlink target mode becoming `0777`.

Attack sides: compromised local user against local client service, malicious local process racing IPC path creation.

Risk: local privilege boundary weakening, IPC spoofing, or permission broadening if a path is attacker-controlled.

Recommendations:
- Create IPC/runtime directories under a private `0700` parent.
- Refuse symlink components using `lstat`/`openat` style no-follow checks.
- Apply permissions to already-open file descriptors where the platform supports it.
- Avoid chmod on attacker-controllable paths.

### SEC-003: Compressed data can expand by very high ratios

`zstd_expansion_probe` compressed 8 MiB of zero bytes to 275 bytes and decompressed it back to 8 MiB, ratio `30504.03`.

Attack sides: client from server, server from client, malicious peer, MITM before authenticated encryption, relay abuse.

Risk: CPU and memory denial of service from small inbound payloads.

Recommendations:
- Decompress through a streaming reader with a hard output byte budget.
- Track compressed-to-decompressed ratio per frame and per peer.
- Reject decompression for unauthenticated or low-trust message types unless explicitly needed.
- Add per-peer decompression CPU/time accounting.

### SEC-004: Server lockfile is not reproducible under `--locked`

`rustdesk-server` locked `cargo tree`, locked `cargo metadata`, and `cargo deny --locked check` fail because Cargo wants to update `Cargo.lock`.

Risk: CI and release builds can resolve a dependency graph different from the reviewed graph.

Recommendations:
- Regenerate and review `rustdesk-server/Cargo.lock` in a dedicated dependency PR.
- Make `cargo metadata --locked` mandatory before release.
- Use the locked advisory runner to prevent silent lockfile mutation.

### SEC-005: RustSec advisories are present in both client and server graphs

Client `cargo audit`: 16 vulnerabilities, 39 allowed warnings.

Server `cargo audit`: 18 vulnerabilities, 21 allowed warnings.

Priority dependencies include `bytes`, TLS stack crates, `protobuf`, `sqlx`, `tungstenite`, `axum`, `quinn`, `libgit2-sys`, and `users`.

Recommendations:
- Upgrade direct high-risk crates first.
- Add `deny.toml` with explicit advisory handling.
- Treat `users` carefully because there is no safe fixed upgrade for one advisory.

### SEC-006: JS/UI supply-chain checks are incomplete

Server UI `npm audit` was skipped because no `package-lock.json` or `npm-shrinkwrap.json` exists. `osv-scanner` was also not installed.

Recommendations:
- Add and commit a package lock if the server UI is shipped.
- Add OSV scanner to CI if you want non-RustSec advisory coverage.

### SEC-007: No pre-existing fuzz suite found

No existing `cargo-fuzz`, AFL, honggfuzz, or `fuzz_targets` suite was found in the client or server repositories.

Recommendations:
- Keep the new `rustdesk-tests/fuzz` targets and run them in scheduled CI.
- Seed corpora from real rendezvous, message, file-transfer, and codec traffic after removing secrets.
