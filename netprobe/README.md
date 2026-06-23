# RustAdmin Net Probe

`rustadmin-netprobe` is a small TCP transport probe for reproducing video-frame
burst behavior without building or running the full RustAdmin client.

It sends synthetic frame payloads split into fixed-size application chunks and
writes JSONL logs on both sides. Run the server on the receiving PC and the
client on the sending PC. Reverse the roles to test the opposite direction.

## Build

```bash
./rustdesk-tests/scripts/build_netprobe.sh
```

On Windows PowerShell:

```powershell
.\rustdesk-tests\scripts\build_netprobe.ps1
```

## Run

Receiver:

```bash
rustadmin-netprobe server --bind 0.0.0.0:23000 --log receiver.jsonl
```

Sender, matching the recent 41-43 chunk RustAdmin frame size:

```bash
rustadmin-netprobe client --connect 192.168.10.5:23000 --duration-sec 30 --fps 30 --frame-size 43090 --chunk-size 1024 --mode burst --log sender.jsonl
```

Paced mode inserts a small delay after every N chunks:

```bash
rustadmin-netprobe client --connect 192.168.10.5:23000 --mode paced --pace-every 4 --pace-us 1000
```

Window mode limits unacknowledged frame bytes:

```bash
rustadmin-netprobe client --connect 192.168.10.5:23000 --mode window --window-bytes 262144 --pace-every 4 --pace-us 1000
```

To reproduce a RustAdmin-style “video starts, then receiver stops draining for a
while” failure, pause receiver reads after a few seconds:

```bash
rustadmin-netprobe server --bind 0.0.0.0:23000 --log receiver-pause.jsonl --pause-read-after-ms 5000 --pause-read-duration-ms 15000
rustadmin-netprobe client --connect 192.168.10.5:23000 --duration-sec 40 --fps 30 --frame-size 150000 --chunk-size 1024 --mode burst --io-timeout-ms 15000 --log sender-burst.jsonl
```

The expected burst-mode failure is a `chunk_write_error` after the receiver
pause fills the TCP send path. To validate a non-disconnecting video policy,
switch the sender to window mode and skip video frames while the receiver has
not ACKed enough bytes:

```bash
rustadmin-netprobe client --connect 192.168.10.5:23000 --duration-sec 40 --fps 30 --frame-size 150000 --chunk-size 1024 --mode window --window-bytes 300000 --window-wait-ms 0 --drop-when-window-full --log sender-window-drop.jsonl
```

Client socket reads and writes use a 15 second timeout by default. Override it
with `--io-timeout-ms N`, or pass `0` to disable it.

Useful fields:
- `frame_send_start` / `frame_sent`: client-side frame emission timing.
- `frame_skipped_window_full`: sender intentionally dropped a video frame rather
  than pushing into a full TCP video window.
- `frame_rx_start` / `frame_complete`: receiver-side reassembly timing.
- `frame_ack`: client-side ACK timing and in-flight drain.
- `summary`: sent and ACKed totals.

## UDP Packetized Video Mode

UDP mode sends each synthetic frame as independent datagrams. Missing packets
expire only their frame; later frames are not blocked behind them.

Receiver:

```bash
rustadmin-netprobe udp-server --bind 0.0.0.0:23000 --log udp-receiver.jsonl --read-timeout-ms 5000 --frame-timeout-ms 1000
```

Sender:

```bash
rustadmin-netprobe udp-client --connect 192.168.10.5:23000 --duration-sec 30 --fps 30 --frame-size 43090 --payload-size 1100 --log udp-sender.jsonl
```

For cleaner network measurements, suppress per-frame logs on both sides:

```bash
rustadmin-netprobe udp-server --bind 0.0.0.0:23000 --log udp-receiver.jsonl --quiet-frames
rustadmin-netprobe udp-client --connect 192.168.10.5:23000 --duration-sec 30 --fps 30 --frame-size 43090 --payload-size 1100 --quiet-frames --log udp-sender.jsonl
```

UDP mode enables compact NACK/retransmit and receiver status packets by default.
The sender also sends one small frame announcement packet before each frame, so
the receiver can keep parity with the sender's frame sequence even across loss.
Recovery is intentionally non-blocking: the receiver NACKs incomplete frames
that have at least one received video chunk, but fully missing frames are skipped
after `--frame-timeout-ms` by default. The sender keeps a rolling cache of recent
chunks and resends requested chunks before the final BYE:

```bash
rustadmin-netprobe udp-server --bind 0.0.0.0:23000 --log udp-receiver-nack.jsonl --read-timeout-ms 60000 --frame-timeout-ms 5000 --quiet-frames
rustadmin-netprobe udp-client --connect 192.168.10.5:23000 --duration-sec 30 --fps 30 --frame-size 43090 --payload-size 1100 --pace-every 1 --pace-us 200 --quiet-frames --log udp-sender-nack.jsonl
```

Use `--nack-rounds 0` on the receiver to disable retransmit requests.
Use `--nack-empty-frames` only for A/B testing full-frame recovery bursts.
Use `--no-announce` on the sender only for A/B testing the old behavior.
Use `--status-interval-ms 0` on the receiver to disable status packets.

To simulate deterministic packet loss on the receiver, use `--drop-every N`.
For example, `--drop-every 40` drops every 40th received UDP packet.

To simulate a harder case where every initial video chunk for a frame is lost
but retransmits can still arrive, use `--drop-initial-frame-video-every N` on
the receiver. For example, `--drop-initial-frame-video-every 5` drops the first
full video packet set for frames 0, 5, 10, and so on.
