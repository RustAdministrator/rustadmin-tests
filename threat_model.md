# RustAdmin / RustDesk Repository Threat Model

## Overview

This repository bundle contains a RustAdmin fork of RustDesk with multiple runtime products:

- `rustdesk/`: the desktop/mobile remote access client and host, including local service code, peer connection handling, screen/audio/input/clipboard/file-transfer features, update/configuration paths, Flutter UI, and legacy Sciter UI.
- `rustadmin-server/`: the self-hosted rendezvous (`hbbs`), relay (`hbbr`), and utility binaries used to broker client discovery, NAT traversal, relay traffic, and optional HTTP/API/admin surfaces.
- `hbb_common/`: shared protocol, protobuf, networking, TLS/websocket, configuration, file-transfer, compression, and byte-framing utilities used on both client and server sides.
- `hwcodec/`: native C/C++/Rust hardware codec bindings and sample capture/encode/decode code.
- `rustadmin-tests/`: defensive fuzz, advisory, dynamic, and resource probes for parser, codec, path, IPC, decompression, and online server request behavior.

The intended secure deployment model is self-hosted and operator-controlled, preferably LAN/VPN-first. Public internet exposure is a higher-risk mode because server ports and client connection paths become reachable by arbitrary hosts, botnets, scanners, and resource-exhaustion traffic.

The primary security goal is safe remote administration: clients must remain safe when the rendezvous/relay/API infrastructure is malicious or compromised; servers must remain safe when clients are malicious, compromised, buggy, or internet-exposed; and both sides must degrade safely under hostile network traffic.

## Threat Model, Trust Boundaries, and Assumptions

Important assets and privileges:

- Remote control authority over keyboard, mouse, screen, audio, clipboard, file transfer, port forwarding, printer, shell/terminal, privacy mode, and service control.
- Client local system privileges, including elevated service paths on Windows/macOS/Linux and platform APIs for screen capture, input simulation, clipboard, IPC, and session switching.
- Server availability, CPU, memory, file descriptors, database integrity, relay bandwidth, and cryptographic identity material.
- Rendezvous/relay/API configuration, ID routing, peer address hints, relay assignment, TLS/websocket/ICE fallback settings, direct-access settings, approval/trust controls, whitelist state, and update metadata.
- Operator secrets such as private keys, API tokens, passwords, TOTP secrets, JWT signing keys, database contents, and logs that may contain peer IDs or addresses.

Main trust boundaries:

- Client to rendezvous server: the server can supply peer status, relay choices, address hints, NAT traversal data, and configuration-like data. In this fork, those values are not inherently trusted.
- Client to relay server: the relay handles attacker-controlled byte streams from peers and may be malicious or compromised. It should not gain plaintext control authority unless the endpoint protocol explicitly allows it.
- Client to peer client/host: each side may be malicious after connection establishment. File transfer, clipboard, port forwarding, terminal, input, and display messages are attacker-controlled at the protocol boundary.
- Internet to server: all publicly bound rendezvous, relay, websocket, HTTP/API, UDP, and TCP listeners receive unauthenticated traffic from arbitrary sources.
- Local unprivileged process to client/service: IPC paths, local sockets, config files, plugins, update helpers, service management, and desktop integration cross OS privilege and user-session boundaries.
- Build/update supply chain to runtime: git dependencies, native codec code, Flutter/npm dependencies, generated protobuf, updater metadata, and packaging scripts affect shipped binaries.

Assumptions:

- A self-hosted operator may control server binaries and deployment config, but a client must not assume the server operator is benign for safety-sensitive remote configuration, peer address hints, or update/configuration data.
- End-to-end peer authentication/encryption is necessary for confidentiality and remote-control authorization, but unauthenticated pre-session parsing, routing, relay, and handshake code still need strict resource limits.
- LAN/VPN deployments lower internet scanning exposure but do not remove malicious-client, compromised-client, malicious-server, insider, or local malware risks.
- Tests, examples, and documentation are lower priority unless they affect shipped runtime behavior, CI release gates, generated artifacts, or operator security decisions.

## Attack Surface, Mitigations, and Attacker Stories

High-impact runtime surfaces:

- Framing and protobuf decode: `hbb_common/src/bytes_codec.rs`, generated protobuf modules, rendezvous and session message parsing.
- Transport and trust setup: `hbb_common/src/tcp.rs`, `hbb_common/src/udp.rs`, `hbb_common/src/tls.rs`, `hbb_common/src/websocket.rs`, `hbb_common/src/socket_client.rs`, and client/server connection orchestration.
- Rendezvous and relay servers: `rustadmin-server/src/rendezvous_server.rs`, `rustadmin-server/src/relay_server.rs`, `rustadmin-server/src/peer.rs`, `rustadmin-server/src/database.rs`, and `rustadmin-server/src/hbbr.rs`.
- Client host/session handling: `rustdesk/src/client.rs`, `rustdesk/src/client/io_loop.rs`, `rustdesk/src/server/`, `rustdesk/src/ipc.rs`, `rustdesk/src/flutter_ffi.rs`, `rustdesk/src/port_forward.rs`, and platform-specific modules.
- File transfer and filesystem handling: `hbb_common/src/fs.rs` and client file-transfer call sites.
- Compression and media decode: `hbb_common/src/compress.rs`, `hbb_common/src/stream.rs`, `hwcodec/src/`, and C/C++ codec bindings.
- Configuration/update surfaces: `hbb_common/src/config.rs`, `rustdesk/src/updater.rs`, API/server URL handling, relay resolution, and remote option propagation.
- Supply chain and packaging: `Cargo.toml`/`Cargo.lock` files, git dependencies, Flutter `pubspec.yaml`, server UI package metadata, Docker/systemd/kubernetes/debian packaging, and platform build scripts.

Existing mitigations and hardening direction:

- The fork explicitly prioritizes self-hosted, LAN/VPN-first operation and safer defaults.
- Project guidance calls out security-sensitive remote configuration values that should not be casually overwritten by remote configuration.
- Rendezvous-provided peer address hints are intended to be validated before use; unsafe loopback, link-local, multicast, or otherwise inappropriate hints should be ignored or trigger safer fallback.
- `rustadmin-tests` contains fuzz targets and dynamic probes for byte codec decode, rendezvous/message protobuf decode, file-transfer path validation, zstd decompression, IPC path handling, and online rendezvous request load.
- Prior baseline testing showed file-transfer path traversal handling rejecting relative traversal, absolute paths, null bytes, and symlink components in the tested helper.

Realistic attacker stories:

- A malicious or compromised rendezvous server attempts to redirect clients to private/loopback/link-local peer addresses, downgrade transport choices, overwrite security-sensitive client settings, or force relay/websocket/TLS fallback behavior that weakens confidentiality, availability, or operator policy.
- A malicious relay or peer sends tiny frame headers that advertise large bodies, compressed bombs, malformed protobuf, websocket/TCP/UDP floods, or many half-open sessions to exhaust memory, CPU, queues, or file descriptors.
- A compromised client uses valid-looking rendezvous or relay protocol messages to register many peers, churn NAT/rendezvous state, abuse relay bandwidth, trigger amplification, poison database state, or attack admin/API surfaces.
- A malicious remote peer uses file transfer, clipboard, port forwarding, terminal, printing, or input events to escape the intended remote-control authorization boundary, overwrite local files, leak secrets, execute local commands, or pivot into local networks.
- Local malware or another user on the same host races or spoofs IPC/runtime directories, service sockets, update helpers, config files, plugins, or permission changes to cross from unprivileged user context into the privileged RustAdmin service.
- A supply-chain attacker compromises git dependencies, stale vulnerable crates, build scripts, native codec code, Flutter/npm packages, or update metadata to affect shipped clients or servers.

Lower-priority or out-of-scope stories:

- Attacks requiring the operator to intentionally run an untrusted modified binary are lower priority than network-reachable or local privilege-boundary bugs, unless the build/update path allows silent replacement.
- Documentation-only mistakes are low priority unless they lead operators to expose unsafe public services or disable key controls.
- Pure denial-of-service against a single already-compromised endpoint is lower priority than remotely triggerable server/client compromise, cross-tenant/server-wide DoS, or bugs that persistently weaken configuration.

## Severity Calibration

Critical:

- Remote code execution, authentication bypass, or remote-control authorization bypass from unauthenticated internet traffic or from a malicious rendezvous/relay/peer.
- Server-side compromise that exposes private keys, JWT/password material, database contents, or lets a malicious client control other clients.
- Client-side compromise where a server admin, relay, or peer can execute code or silently grant remote-control capability without explicit local approval/policy.
- Update/configuration compromise that lets untrusted infrastructure change trusted server, relay, API, TLS, approval, whitelist, or direct-access policy.

High:

- Low-bandwidth unauthenticated memory/CPU/file-descriptor exhaustion against public servers or clients before authentication.
- Relay/rendezvous amplification or bandwidth abuse that can be triggered by arbitrary internet hosts or compromised clients.
- File-transfer, path traversal, symlink, IPC, or local service bugs that cross user/service privilege boundaries or overwrite sensitive files.
- Broken peer address validation that causes clients to connect to loopback/link-local/private targets controlled by a malicious server, enabling SSRF-like local-network probing or policy bypass.
- Dependency advisories in parser, TLS, websocket, protobuf, crypto, SQL, HTTP, updater, or native codec stacks when the vulnerable code path is reachable in shipped clients or servers.

Medium:

- Authenticated or local-only denial-of-service with bounded blast radius.
- Bugs that weaken operator visibility, logging, approval prompts, or safe defaults but still require user confirmation or valid credentials.
- Database integrity issues limited to one client record or one session without cross-client control.
- Security test/CI gaps that could let regressions ship but are not themselves directly exploitable.

Low:

- Developer-only tool issues, example-only flaws, stale docs, or non-shipped UI dependency issues without release impact.
- Theoretical parser edge cases that require unrealistic payload sizes, local debug flags, or disabled production controls.
- Cosmetic UI or logging bugs that do not affect security decisions, secrets, or operator response.
