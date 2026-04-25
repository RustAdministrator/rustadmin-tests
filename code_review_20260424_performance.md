# RustDesk Performance Review Notes - Safety Tests

Generated: 2026-04-25

Scope: performance and resource-risk issues surfaced by standalone safety tests. The focus is remote-control behavior over LTE/4G, VPN, and LAN links.

## Findings

### PERF-001: Header-only traffic can trigger large allocation

`codec_header_alloc_probe` shows a 64 MiB capacity growth from a tiny header. This is both a security and performance issue.

Impact:
- High allocator pressure on low-bandwidth links.
- Poor behavior under many slow peers.
- Increased memory fragmentation risk.

Recommendations:
- Reject oversized frames before reserving.
- Use separate caps for control, file-transfer, video/audio, relay, and unauthenticated traffic.
- Add counters for rejected frame length, bytes reserved, and active per-peer buffer memory.

### PERF-002: Decompression needs output budgets

`zstd_expansion_probe` produced a `30504.03` expansion ratio.

Impact:
- Small network packets can create large memory writes and CPU work.
- Relay/server paths can be abused for amplification-like resource burn.

Recommendations:
- Stream decompression into bounded buffers.
- Use explicit output limits per message type.
- Track decompression CPU and output bytes per peer/session.

### PERF-003: Dependency graph drift increases binary and runtime cost

The advisory output shows a large graph with multiple stale or vulnerable transitive crates. This usually also means extra compile time, larger binaries, and more duplicated code paths.

Recommendations:
- Use `cargo tree -d --locked` as a periodic performance hygiene check.
- Consolidate TLS, HTTP, protobuf, and async-stack versions where feasible.
- Avoid dependency upgrades that pull in heavier stacks without measuring binary size and startup time.

### PERF-004: Fuzz smoke validates parsers, not throughput

The current fuzz smoke proves parser robustness for short runs, but it does not measure hot-path latency or allocation counts.

Recommendations:
- Add microbenchmarks for frame decode, protobuf decode, file-transfer path validation, compression/decompression, and relay routing.
- Add allocation counters around packet decode and session hot loops.
- Benchmark under simulated LTE/VPN latency and packet loss, not only localhost.

### PERF-005: Backpressure should be explicit at every trust boundary

The reviewed probes point to a common need: unauthenticated and low-trust inputs must not get unbounded memory, CPU, or queue capacity.

Recommendations:
- Define per-peer budgets for memory, queued messages, decompression output, and expensive auth/rendezvous requests.
- Make queue drops visible through metrics and logs.
- Prefer bounded channels in hot paths and document why any unbounded channel is safe.
