# Security Review - RustDesk / RustAdmin Fork - 2026-05-06

## Scope

Reviewed the local multi-repo checkout rooted at `/Users/s02299/GH/rustdesk`: `rustdesk`, `rustadmin-server`, `hbb_common`, `hwcodec`, and `rustadmin-tests`. The pass prioritized real attack paths for a hardened self-hosted remote access deployment:

- malicious or compromised rendezvous/relay server against clients
- malicious or compromised clients against servers
- unauthenticated internet traffic against servers and clients
- local compromise/infrastructure hardening issues on server and client hosts

This was a high-impact audit, not a complete line-by-line proof over every file. I enumerated 819 Rust/TOML/lock/C/C++/header files and focused deep inspection on network entry points, framing, compression, relay/rendezvous control flow, file transfer, IPC, dependency locks, and codec wrappers.

## Executive Summary

The fork already contains meaningful hardening compared with the April baseline: framed packet header preallocation is fixed, IPC directories/sockets are much tighter, unsafe rendezvous peer-address hints are filtered, server peer/relay/rate structures are bounded, and the primary Rust lockfiles are currently reproducible with `cargo metadata --locked`.

The highest real residual risks are:

1. unbounded zstd decompression on peer-controlled clipboard/file/terminal payloads
2. an old server WebSocket dependency with a known unauthenticated remote DoS advisory
3. a server `OnlineRequest` cap bug that limits lookup but still allocates/sends based on attacker input
4. unsafe trust of forwarded proxy headers when `TRUST_PROXY_HEADERS` is enabled
5. unauthenticated loopback admin command sockets on server ports
6. unchecked unsafe codec wrapper metadata when decoding remote video bitstreams

## Findings

### SEC-001 - High - Unbounded zstd decompression on peer-controlled payloads

**Attack vectors:** malicious/compromised host or client against the peer; malicious server/admin if they can cause or inject peer payloads; availability attack against clients and services.

`hbb_common/src/compress.rs:32-33` exposes:

```rust
pub fn decompress(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap_or_default()
}
```

There is no output size limit, no expansion-ratio cap, no streaming budget, and no error propagation. This is reachable from multiple peer-controlled paths:

- file transfer blocks: `rustdesk/src/server/connection.rs:3134-3141` forwards remote `FileResponse::Block`; `hbb_common/src/fs.rs:810-817` decompresses and writes it
- clipboard: `rustdesk/src/clipboard.rs:1327-1353`, `rustdesk/src/clipboard.rs:1401-1424`, plus iOS paths in `rustdesk/src/server/connection.rs:2762-2774` and `rustdesk/src/client/io_loop.rs:1438-1449`
- terminal output surfaced to Flutter: `rustdesk/src/flutter.rs:1145-1153`

**Impact:** a small compressed input can expand into large memory and CPU use, killing the UI/session/service. The April safety baseline recorded 275 bytes expanding to 8 MiB; the current local probe cannot run because the test harness manifest is broken, but the production decompressor remains uncapped.

**Fix:** replace `decompress()` with a budgeted API such as `decompress_limited(data, max_output, context) -> Result<Vec<u8>>`; use separate limits for clipboard text/image, file blocks, terminal output, and any future compressed message; reject excessive expansion ratios; propagate decompression errors instead of returning an empty vector.

### SEC-002 - High - Server WebSocket stack still uses vulnerable `tungstenite 0.17.2`

**Attack vectors:** unauthenticated internet traffic against `hbbs`/`hbbr` WebSocket ports.

`rustadmin-server/Cargo.toml:44-45` directly pins:

- `tokio-tungstenite = "0.17"`
- `tungstenite = "0.17"`

`rustadmin-server/Cargo.lock` resolves this to `tokio-tungstenite 0.17.1` and `tungstenite 0.17.2`. The server accepts WebSocket handshakes in `rustadmin-server/src/rendezvous_server.rs:1413-1421` and `rustadmin-server/src/relay_server.rs:543-550`.

RustSec advisory [RUSTSEC-2023-0065](https://rustsec.org/advisories/RUSTSEC-2023-0065.html) says affected `tungstenite` versions before `0.20.1` can be driven into remote CPU denial of service during client handshake parsing. The advisory is CVSS 7.5, network, low complexity, no privileges.

**Impact:** an unauthenticated internet client can spend server CPU on WebSocket handshake parsing, bypassing most application-level checks because the vulnerable parsing is before protocol authentication.

**Fix:** upgrade RustAdmin server to `tokio-tungstenite`/`tungstenite >= 0.20.1`, preferably aligning with the `hbb_common` `0.26` stack already present in the combined lockfile. Add a dependency policy gate so old WebSocket crates cannot be reintroduced.

### SEC-003 - Medium-High - `OnlineRequest` cap limits lookup but not allocation/response size

**Attack vectors:** malicious client against rendezvous server; unauthenticated internet if server key is empty or leaked.

`rustadmin-server/src/rendezvous_server.rs:996-1028` computes `peer_lookup_limit`, but allocates and sends response state using the original untrusted `peers.len()`:

- cap calculation: `peer_lookup_limit = clamped_online_request_peer_count(peers.len())` at line 1001
- oversized allocation: `BytesMut::zeroed((peers.len() + 7) / 8)` at line 1010
- capped loop only uses `.take(peer_lookup_limit)` at line 1011

The default cap is 4096 peers, but the response byte vector still scales with the attacker-supplied list length.

**Impact:** the intended `MAX_ONLINE_REQUEST_PEERS` control is partly bypassed, allowing avoidable memory allocation and response bandwidth. The frame limit bounds the absolute size, so this is not unlimited, but it is a real server DoS/cost issue.

**Fix:** reject over-cap requests or truncate before allocation. Allocate `states` from `peer_lookup_limit`, not `peers.len()`. Add a regression test that sends more than the cap and asserts the response state length is no larger than `(cap + 7) / 8`.

### SEC-004 - Medium - `TRUST_PROXY_HEADERS` trusts spoofable forwarded headers from any WebSocket client

**Attack vectors:** direct internet clients against server deployments using reverse-proxy mode.

`rustadmin-server/src/common.rs:132-142` enables proxy header trust through `TRUST_PROXY_HEADERS`. When enabled, `apply_trusted_proxy_addr()` in `rustadmin-server/src/common.rs:350-370` unconditionally trusts `X-Real-IP` or the first `X-Forwarded-For` value. This is used during WebSocket accept in:

- `rustadmin-server/src/rendezvous_server.rs:1413-1429`
- `rustadmin-server/src/relay_server.rs:543-560`

There is no trusted proxy source allowlist. If the WebSocket port is reachable directly, or the proxy passes client-supplied forwarded headers through, any client can spoof their rate-limit/audit IP.

**Impact:** bypasses per-IP rate limits, blocklists, relay accounting, and logs. This is realistic in self-hosted deployments because operators often expose WebSocket behind nginx/Caddy/Traefik and may enable the environment variable without source-IP enforcement.

**Fix:** only honor forwarded headers when the TCP peer IP is in an explicit trusted proxy allowlist. Prefer proxy-side header overwrite, not pass-through. Consider PROXY protocol or a trusted local socket between proxy and service.

### SEC-005 - Medium - Unauthenticated loopback admin command interfaces on server ports

**Attack vectors:** local unprivileged process on server host; SSRF-to-loopback in adjacent infrastructure; compromised sidecar/container on same host.

The rendezvous NAT listener treats loopback TCP connections as admin commands without an auth token:

- dispatch: `rustadmin-server/src/rendezvous_server.rs:1347-1363`
- commands: `rustadmin-server/src/rendezvous_server.rs:1132-1325`

Commands can read operational state and mutate runtime settings such as relay server list and `ALWAYS_USE_RELAY`.

The relay server has similar loopback command handling:

- dispatch: `rustadmin-server/src/relay_server.rs:502-522`
- commands: `rustadmin-server/src/relay_server.rs:243-459`

Those commands include blacklist/blocklist and bandwidth/session controls.

**Impact:** not remotely exploitable by itself, but on a shared server, compromised sidecar, or SSRF-capable local service, an attacker can change live RustAdmin server behavior and weaken availability/routing controls.

**Fix:** disable these interfaces by default, or move them to a root-owned Unix socket with `0600` permissions. If TCP is kept, require an admin token and bind an explicit admin port.

### SEC-006 - Medium - Unsafe decoder wrapper trusts dimensions, linesizes, and pointers from remote video decode

**Attack vectors:** malicious/compromised remote host/server against viewer clients; malformed video bitstream reaching FFmpeg/hardware decoder callbacks.

`hwcodec/src/ffmpeg_ram/decode.rs:108-157` builds Rust slices from raw C callback values. It trusts `width`, `height`, `pixfmt`, `linesizes`, and `datas` without checking for null pointers, negative values, integer overflow, or maximum frame size:

- YUV420P sizes: lines 130-133
- NV12 sizes: lines 144-146

For example, `(linesizes[0] * height) as usize` can wrap a negative or overflowing `i32` product into a huge `usize`, and `from_raw_parts(...).to_vec()` can read outside the decoder-owned buffer.

**Impact:** if a malformed remote video stream or decoder edge case produces hostile callback metadata, the client can panic, OOM, or perform undefined behavior while viewing a remote system. This is a hardening issue around a high-risk parsing boundary.

**Fix:** reject null data pointers; require positive width/height/linesizes; use checked multiplication; enforce maximum decoded frame bytes; return an error rather than pushing a frame when metadata is invalid. Add fuzz/sanitizer coverage around malformed H.264/H.265 packets and direct unit tests for callback validation via a safe wrapper.

### SEC-007 - Medium-Low - Server `id_ed25519` private key is created without restrictive mode

**Attack vectors:** local users/processes on the server host; backup/container leakage.

`rustadmin-server/src/common.rs:382-421` creates `id_ed25519` with `std::fs::File::create(sk_file)` at line 419. On Unix, this relies on process umask and does not use `create_new`, `0o600`, or post-create permission enforcement.

**Impact:** with a permissive umask, the server private key can become group/world-readable. That weakens server identity and can enable impersonation or offline key theft depending on deployment and client trust model.

**Fix:** create the secret with `OpenOptions::new().write(true).create_new(true).mode(0o600)`, refuse or repair too-open existing keys, and ensure the containing directory is private.

### SEC-008 - Low/Medium-Low - Current TLS dependency lock contains `rustls-webpki` versions below current RustSec patched ranges

**Attack vectors:** TLS certificate-validation edge cases; mostly requires CA misissuance or CRL use.

Current lock versions:

- `rustadmin-server/Cargo.lock`: `rustls-webpki 0.103.11` and `0.101.7`
- `rustdesk/Cargo.lock`: `rustls-webpki 0.103.3`

Current RustSec entries for `rustls-webpki` include:

- [RUSTSEC-2026-0098](https://rustsec.org/advisories/RUSTSEC-2026-0098.html), patched in `>=0.103.12`
- [RUSTSEC-2026-0099](https://rustsec.org/advisories/RUSTSEC-2026-0099.html), patched in `>=0.103.12`
- [RUSTSEC-2026-0104](https://rustsec.org/advisories/RUSTSEC-2026-0104.html), patched in `>=0.103.13`
- [RUSTSEC-2026-0049](https://rustsec.org/advisories/RUSTSEC-2026-0049), patched in `>=0.103.10` and unaffected below `0.102.0-alpha.0`

These are lower priority than direct unauthenticated server DoS issues because the practical exploit conditions are narrower, but TLS dependencies are part of the trust boundary for a hardened remote access system.

**Fix:** upgrade transitive TLS stack packages so all `0.103.x` `rustls-webpki` resolutions are at least `0.103.13`, then run `cargo audit`/`cargo deny`.

### SEC-009 - Low - Client lock contains `rand 0.9.2`, below the current advisory patched line

`rustdesk/Cargo.lock` contains `rand 0.9.2`. RustSec [RUSTSEC-2026-0097](https://rustsec.org/advisories/RUSTSEC-2026-0097.html) patches the `0.9` line at `>=0.9.3`. The advisory requires a custom logger and specific thread RNG re-entry conditions, so this is not a primary remote threat in this codebase.

**Fix:** move to `rand >=0.9.3` where dependency constraints allow.

## Suppressed / Resolved Items

- **Header-only framed packet preallocation:** fixed in `hbb_common/src/bytes_codec.rs:5-66`; tests at `hbb_common/src/bytes_codec.rs:301-323` cover over-limit and no-preallocation behavior.
- **IPC directory/socket symlink and `0777` baseline issue:** current code rejects symlink IPC paths and uses `0o700` directories plus `0o600` sockets in `hbb_common/src/config.rs:681-764`, `hbb_common/src/config.rs:1008-1039`, and `rustdesk/src/ipc.rs:410-495`.
- **Malicious rendezvous peer-hint to loopback/link-local:** current client code rejects unsafe direct/local hints in `rustdesk/src/common.rs:1337-1384` and `rustdesk/src/client.rs:549-583`.
- **Basic server resource caps:** current server has per-IP TCP/UDP/control limits, relay pending/active caps, session timeouts, peer-cache caps, and SQLx-bound queries. See `rustadmin-server/src/common.rs:20-306`, `rustadmin-server/src/relay_server.rs:64-787`, `rustadmin-server/src/peer.rs:25-382`, and `rustadmin-server/src/database.rs:137-216`.
- **Server lockfile reproducibility:** `cargo metadata --locked` succeeded for `rustadmin-server`, `rustdesk`, and `hbb_common`.

## Verification Performed

- `cargo metadata --locked --manifest-path rustadmin-server/Cargo.toml --no-deps`
- `cargo metadata --locked --manifest-path rustdesk/Cargo.toml --no-deps`
- `cargo metadata --locked --manifest-path hbb_common/Cargo.toml --no-deps`
- targeted source review of rendezvous/relay server listeners, peer map, DB access, IPC, compression, file transfer, clipboard, terminal response, dependency locks, and `hwcodec` decode wrappers
- current advisory lookup from RustSec for `tungstenite`, `rustls-webpki`, and `rand`

## Verification Gaps

- `cargo audit` is not installed locally.
- `cargo deny` is not installed locally.
- `rustadmin-tests` safety/fuzz harnesses do not build in this checkout because their `hbb_common` path points to a nonexistent `rustdesk-client/libs/hbb_common`.
- I did not run full client/server integration tests or sanitizer/fuzzer jobs.
