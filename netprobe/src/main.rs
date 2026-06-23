use std::collections::BTreeMap;
use std::env;
use std::fmt::{self, Write as FmtWrite};
use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Read, Write as IoWrite};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: u32 = 0x5241_4e50; // RANP
const VERSION: u16 = 1;
const MSG_HELLO: u8 = 1;
const MSG_CHUNK: u8 = 2;
const MSG_FRAME_ACK: u8 = 3;
const MSG_BYE: u8 = 4;
const UDP_PACKET_VIDEO: u8 = 1;
const UDP_PACKET_BYE: u8 = 2;
const UDP_PACKET_NACK: u8 = 3;
const UDP_PACKET_ANNOUNCE: u8 = 4;
const UDP_PACKET_STATUS: u8 = 5;
const UDP_PACKET_HEADER_LEN: usize = 47;
const UDP_STATUS_BODY_LEN: usize = 44;
const HELLO_BODY_LEN: usize = 27;
const CHUNK_BODY_LEN: usize = 32;
const ACK_BODY_LEN: usize = 41;
const MAX_RECORD_LEN: usize = 16 * 1024 * 1024;
const MAX_UDP_PACKET_LEN: usize = 65_507;
const MAX_SYNTHETIC_GAP_FRAMES: u64 = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendMode {
    Burst,
    Paced,
    Window,
}

impl SendMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "burst" => Ok(Self::Burst),
            "paced" => Ok(Self::Paced),
            "window" => Ok(Self::Window),
            _ => Err(format!(
                "unknown mode '{value}', expected burst, paced, or window"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Burst => "burst",
            Self::Paced => "paced",
            Self::Window => "window",
        }
    }

    fn wire(self) -> u8 {
        match self {
            Self::Burst => 0,
            Self::Paced => 1,
            Self::Window => 2,
        }
    }

    fn from_wire(value: u8) -> &'static str {
        match value {
            0 => "burst",
            1 => "paced",
            2 => "window",
            _ => "unknown",
        }
    }
}

#[derive(Debug)]
enum Command {
    Server(ServerConfig),
    Client(ClientConfig),
    UdpServer(UdpServerConfig),
    UdpClient(UdpClientConfig),
}

#[derive(Debug)]
struct ServerConfig {
    bind: String,
    log_path: Option<String>,
    ack_every_chunks: u32,
    read_timeout_ms: u64,
    pause_read_after_ms: u64,
    pause_read_duration_ms: u64,
    pause_read_repeat_ms: u64,
    verbose_chunks: bool,
    tcp_nodelay: bool,
}

#[derive(Debug)]
struct ClientConfig {
    connect: String,
    log_path: Option<String>,
    duration_sec: u64,
    fps: u32,
    frame_size: u32,
    chunk_size: u32,
    mode: SendMode,
    window_bytes: u64,
    window_wait_ms: u64,
    drop_when_window_full: bool,
    pace_every: u32,
    pace_us: u64,
    io_timeout_ms: u64,
    verbose_chunks: bool,
    tcp_nodelay: bool,
}

#[derive(Debug)]
struct UdpServerConfig {
    bind: String,
    log_path: Option<String>,
    read_timeout_ms: u64,
    frame_timeout_ms: u64,
    drop_every: u64,
    drop_initial_frame_video_every: u64,
    verbose_chunks: bool,
    quiet_frames: bool,
    nack_delay_ms: u64,
    nack_interval_ms: u64,
    nack_rounds: u32,
    nack_max_chunks: u32,
    nack_empty_frames: bool,
    status_interval_ms: u64,
}

#[derive(Debug)]
struct UdpClientConfig {
    connect: String,
    bind: String,
    log_path: Option<String>,
    duration_sec: u64,
    fps: u32,
    frame_size: u32,
    payload_size: u32,
    pace_every: u32,
    pace_us: u64,
    verbose_chunks: bool,
    quiet_frames: bool,
    resend_cache_frames: usize,
    nack_linger_ms: u64,
    announce_frames: bool,
}

#[derive(Debug)]
struct HelloMsg {
    chunk_size: u32,
    frame_size: u32,
    fps: u32,
    duration_sec: u32,
    window_bytes: u32,
    mode: u8,
}

#[derive(Debug)]
struct ChunkMsg {
    frame_id: u64,
    chunk_index: u32,
    chunk_count: u32,
    frame_size: u32,
    sent_unix_us: u64,
    payload_len: u32,
}

#[derive(Debug)]
struct FrameAckMsg {
    frame_id: u64,
    chunks_seen: u32,
    chunk_count: u32,
    frame_size: u32,
    bytes_seen: u32,
    complete_unix_us: u64,
    receive_elapsed_us: u64,
    complete: bool,
}

#[derive(Debug)]
enum WireMessage {
    Hello(HelloMsg),
    Chunk(ChunkMsg),
    FrameAck(FrameAckMsg),
    Bye,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UdpVideoPacketHeader {
    packet_type: u8,
    session_id: u64,
    frame_id: u64,
    chunk_index: u32,
    chunk_count: u32,
    frame_size: u32,
    payload_len: u32,
    sent_unix_us: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UdpStatusMsg {
    last_observed_frame_id: u64,
    frames_complete: u64,
    frames_skipped: u64,
    open_frames: u32,
    chunks_total: u64,
    announce_packets_total: u64,
}

#[derive(Default)]
struct FlowState {
    inflight_bytes: u64,
    acked_frames: u64,
    acked_bytes: u64,
    acked_frame_bytes: BTreeMap<u64, u64>,
}

struct FrameRxState {
    session_id: u64,
    frame_size: u32,
    chunk_count: u32,
    first_unix_us: u64,
    started: Instant,
    seen: Vec<bool>,
    chunks_seen: u32,
    bytes_seen: u32,
    last_nack: Option<Instant>,
    nack_rounds: u32,
}

struct SentUdpFrame {
    chunks: Vec<Vec<u8>>,
}

#[derive(Default)]
struct UdpClientStats {
    nacks_received: u64,
    nack_chunks_requested: u64,
    retransmit_packets: u64,
    retransmit_bytes: u64,
    retransmit_misses: u64,
    announce_packets_sent: u64,
    statuses_received: u64,
    status_frames_complete: u64,
    status_frames_skipped: u64,
    status_last_observed_frame_id: u64,
}

struct Logger {
    file: Option<Mutex<BufWriter<File>>>,
}

impl Logger {
    fn new(path: Option<&str>) -> Result<Self, String> {
        let file = match path {
            Some(path) => {
                let file = File::create(path)
                    .map_err(|err| format!("failed to create log '{path}': {err}"))?;
                Some(Mutex::new(BufWriter::new(file)))
            }
            None => None,
        };
        Ok(Self { file })
    }

    fn event(&self, side: &str, event: &str, fields: fmt::Arguments<'_>) {
        let mut line = String::with_capacity(256);
        let _ = write!(
            &mut line,
            "{{\"ts_ms\":{},\"side\":\"{}\",\"event\":\"{}\"",
            unix_ms(),
            side,
            event
        );
        let _ = line.write_fmt(fields);
        line.push('}');
        println!("{line}");
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    match parse_args()? {
        Command::Server(config) => run_server(config),
        Command::Client(config) => run_client(config),
        Command::UdpServer(config) => run_udp_server(config),
        Command::UdpClient(config) => run_udp_client(config),
    }
}

fn run_server(config: ServerConfig) -> Result<(), String> {
    let logger = Arc::new(Logger::new(config.log_path.as_deref())?);
    let listener = TcpListener::bind(&config.bind)
        .map_err(|err| format!("failed to bind '{}': {err}", config.bind))?;
    logger.event(
        "server",
        "listen",
        format_args!(
            ",\"bind\":\"{}\",\"ack_every_chunks\":{},\"pause_read_after_ms\":{},\"pause_read_duration_ms\":{},\"pause_read_repeat_ms\":{},\"tcp_nodelay\":{}",
            json_escape(&config.bind),
            config.ack_every_chunks,
            config.pause_read_after_ms,
            config.pause_read_duration_ms,
            config.pause_read_repeat_ms,
            config.tcp_nodelay
        ),
    );

    let (stream, peer) = listener
        .accept()
        .map_err(|err| format!("failed to accept connection: {err}"))?;
    stream
        .set_nodelay(config.tcp_nodelay)
        .map_err(|err| format!("failed to configure TCP_NODELAY: {err}"))?;
    if config.read_timeout_ms != 0 {
        stream
            .set_read_timeout(Some(Duration::from_millis(config.read_timeout_ms)))
            .map_err(|err| format!("failed to configure read timeout: {err}"))?;
    }
    logger.event(
        "server",
        "accept",
        format_args!(
            ",\"peer\":\"{}\",\"read_timeout_ms\":{}",
            json_escape(&peer.to_string()),
            config.read_timeout_ms
        ),
    );
    let result = handle_server_connection(stream, &config, Arc::clone(&logger));
    if let Err(err) = &result {
        logger.event(
            "server",
            "fatal",
            format_args!(",\"message\":\"{}\"", json_escape(err)),
        );
    }
    result
}

fn handle_server_connection(
    mut stream: TcpStream,
    config: &ServerConfig,
    logger: Arc<Logger>,
) -> Result<(), String> {
    let mut scratch = Vec::with_capacity(64 * 1024);
    let mut frames: BTreeMap<u64, FrameRxState> = BTreeMap::new();
    let mut chunks_total = 0_u64;
    let mut bytes_total = 0_u64;
    let started = Instant::now();
    let mut next_pause_after_ms = if config.pause_read_duration_ms == 0 {
        None
    } else {
        Some(config.pause_read_after_ms)
    };
    let mut pauses_taken = 0_u64;

    loop {
        maybe_pause_server_read(
            &logger,
            config,
            started,
            &mut next_pause_after_ms,
            &mut pauses_taken,
        );
        let message = match read_wire_message(&mut stream, &mut scratch) {
            Ok(Some(message)) => message,
            Ok(None) => {
                logger.event(
                    "server",
                    "eof",
                    format_args!(
                        ",\"chunks_total\":{},\"bytes_total\":{},\"open_frames\":{}",
                        chunks_total,
                        bytes_total,
                        frames.len()
                    ),
                );
                log_open_frames(&logger, "eof", &frames);
                return Ok(());
            }
            Err(err) => {
                logger.event(
                    "server",
                    "read_error",
                    format_args!(
                        ",\"message\":\"{}\",\"chunks_total\":{},\"bytes_total\":{},\"open_frames\":{}",
                        json_escape(&err.to_string()),
                        chunks_total,
                        bytes_total,
                        frames.len()
                    ),
                );
                log_open_frames(&logger, "read_error", &frames);
                return Err(format!("server read failed: {err}"));
            }
        };

        match message {
            WireMessage::Hello(hello) => {
                logger.event(
                    "server",
                    "hello",
                    format_args!(
                        ",\"mode\":\"{}\",\"chunk_size\":{},\"frame_size\":{},\"fps\":{},\"duration_sec\":{},\"window_bytes\":{}",
                        SendMode::from_wire(hello.mode),
                        hello.chunk_size,
                        hello.frame_size,
                        hello.fps,
                        hello.duration_sec,
                        hello.window_bytes
                    ),
                );
            }
            WireMessage::Chunk(chunk) => {
                if chunk.chunk_count == 0 || chunk.chunk_index >= chunk.chunk_count {
                    logger.event(
                        "server",
                        "bad_chunk",
                        format_args!(
                            ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{}",
                            chunk.frame_id, chunk.chunk_index, chunk.chunk_count
                        ),
                    );
                    continue;
                }

                chunks_total += 1;
                bytes_total += u64::from(chunk.payload_len);
                if config.verbose_chunks {
                    logger.event(
                        "server",
                        "chunk",
                        format_args!(
                            ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{},\"payload_len\":{}",
                            chunk.frame_id,
                            chunk.chunk_index,
                            chunk.chunk_count,
                            chunk.payload_len
                        ),
                    );
                }

                if !frames.contains_key(&chunk.frame_id) {
                    let mut seen = Vec::new();
                    seen.resize(chunk.chunk_count as usize, false);
                    frames.insert(
                        chunk.frame_id,
                        FrameRxState {
                            session_id: 0,
                            frame_size: chunk.frame_size,
                            chunk_count: chunk.chunk_count,
                            first_unix_us: chunk.sent_unix_us,
                            started: Instant::now(),
                            seen,
                            chunks_seen: 0,
                            bytes_seen: 0,
                            last_nack: None,
                            nack_rounds: 0,
                        },
                    );
                    logger.event(
                        "server",
                        "frame_rx_start",
                        format_args!(
                            ",\"frame_id\":{},\"chunk_count\":{},\"frame_size\":{},\"first_chunk_index\":{}",
                            chunk.frame_id,
                            chunk.chunk_count,
                            chunk.frame_size,
                            chunk.chunk_index
                        ),
                    );
                }

                let mut ack = None;
                let mut complete = false;
                if let Some(frame) = frames.get_mut(&chunk.frame_id) {
                    if frame.chunk_count != chunk.chunk_count
                        || frame.frame_size != chunk.frame_size
                    {
                        logger.event(
                            "server",
                            "frame_mismatch",
                            format_args!(
                                ",\"frame_id\":{},\"expected_chunks\":{},\"got_chunks\":{},\"expected_size\":{},\"got_size\":{}",
                                chunk.frame_id,
                                frame.chunk_count,
                                chunk.chunk_count,
                                frame.frame_size,
                                chunk.frame_size
                            ),
                        );
                        continue;
                    }

                    let index = chunk.chunk_index as usize;
                    if frame.seen[index] {
                        logger.event(
                            "server",
                            "duplicate_chunk",
                            format_args!(
                                ",\"frame_id\":{},\"chunk_index\":{}",
                                chunk.frame_id, chunk.chunk_index
                            ),
                        );
                    } else {
                        frame.seen[index] = true;
                        frame.chunks_seen += 1;
                        frame.bytes_seen = frame.bytes_seen.saturating_add(chunk.payload_len);
                    }

                    if frame.chunks_seen == frame.chunk_count {
                        complete = true;
                        ack = Some(make_ack(chunk.frame_id, frame, true));
                        logger.event(
                            "server",
                            "frame_complete",
                            format_args!(
                                ",\"frame_id\":{},\"chunks_seen\":{},\"chunk_count\":{},\"bytes_seen\":{},\"sender_first_unix_us\":{},\"elapsed_us\":{}",
                                chunk.frame_id,
                                frame.chunks_seen,
                                frame.chunk_count,
                                frame.bytes_seen,
                                frame.first_unix_us,
                                frame.started.elapsed().as_micros()
                            ),
                        );
                    } else if config.ack_every_chunks != 0
                        && frame.chunks_seen % config.ack_every_chunks == 0
                    {
                        ack = Some(make_ack(chunk.frame_id, frame, false));
                    }
                }

                if let Some(ack) = ack {
                    if let Err(err) = write_ack(&mut stream, &ack) {
                        logger.event(
                            "server",
                            "ack_write_error",
                            format_args!(
                                ",\"frame_id\":{},\"message\":\"{}\"",
                                ack.frame_id,
                                json_escape(&err.to_string())
                            ),
                        );
                        return Err(format!("server ack write failed: {err}"));
                    }
                }
                if complete {
                    frames.remove(&chunk.frame_id);
                }
            }
            WireMessage::FrameAck(_) => {
                logger.event("server", "unexpected_ack", format_args!(""));
            }
            WireMessage::Bye => {
                logger.event(
                    "server",
                    "bye",
                    format_args!(
                        ",\"chunks_total\":{},\"bytes_total\":{},\"open_frames\":{}",
                        chunks_total,
                        bytes_total,
                        frames.len()
                    ),
                );
                let _ = stream.shutdown(Shutdown::Both);
                return Ok(());
            }
        }
    }
}

fn run_udp_server(config: UdpServerConfig) -> Result<(), String> {
    validate_udp_server_config(&config)?;
    let logger = Arc::new(Logger::new(config.log_path.as_deref())?);
    let socket = UdpSocket::bind(&config.bind)
        .map_err(|err| format!("failed to bind UDP '{}': {err}", config.bind))?;
    let nack_enabled = config.nack_rounds != 0;
    let socket_timeout = if nack_enabled {
        Some(Duration::from_millis(config.nack_interval_ms.clamp(1, 100)))
    } else if config.read_timeout_ms != 0 {
        Some(Duration::from_millis(config.read_timeout_ms))
    } else {
        None
    };
    socket
        .set_read_timeout(socket_timeout)
        .map_err(|err| format!("failed to configure UDP read timeout: {err}"))?;
    logger.event(
        "udp_server",
        "listen",
        format_args!(
            ",\"bind\":\"{}\",\"read_timeout_ms\":{},\"frame_timeout_ms\":{},\"drop_every\":{},\"drop_initial_frame_video_every\":{},\"nack_delay_ms\":{},\"nack_interval_ms\":{},\"nack_rounds\":{},\"nack_max_chunks\":{},\"nack_empty_frames\":{},\"status_interval_ms\":{}",
            json_escape(&config.bind),
            config.read_timeout_ms,
            config.frame_timeout_ms,
            config.drop_every,
            config.drop_initial_frame_video_every,
            config.nack_delay_ms,
            config.nack_interval_ms,
            config.nack_rounds,
            config.nack_max_chunks,
            config.nack_empty_frames,
            config.status_interval_ms
        ),
    );

    let mut buf = vec![0_u8; MAX_UDP_PACKET_LEN];
    let mut nack_buf = vec![
        0_u8;
        UDP_PACKET_HEADER_LEN
            + config.nack_max_chunks as usize * std::mem::size_of::<u32>()
    ];
    let mut frames: BTreeMap<u64, FrameRxState> = BTreeMap::new();
    let mut completed_frames: BTreeMap<u64, Instant> = BTreeMap::new();
    let mut initial_frame_video_drop_counts: BTreeMap<u64, u32> = BTreeMap::new();
    let mut peer: Option<SocketAddr> = None;
    let mut active_session_id = None;
    let mut packets_total = 0_u64;
    let mut packets_dropped = 0_u64;
    let mut chunks_total = 0_u64;
    let mut bytes_total = 0_u64;
    let mut frames_complete = 0_u64;
    let mut frames_expired = 0_u64;
    let mut announce_packets_total = 0_u64;
    let mut synthetic_frames_opened = 0_u64;
    let mut nack_packets_sent = 0_u64;
    let mut nack_chunks_requested = 0_u64;
    let mut status_packets_sent = 0_u64;
    let mut last_observed_frame_id = None;
    let mut last_status_at = Instant::now();
    let frame_timeout = Duration::from_millis(config.frame_timeout_ms);
    let completed_retention = Duration::from_millis(config.frame_timeout_ms.saturating_mul(2));
    let mut last_packet_at = Instant::now();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                last_packet_at = Instant::now();
                packets_total = packets_total.saturating_add(1);
                if peer.is_none() {
                    peer = Some(addr);
                    logger.event(
                        "udp_server",
                        "peer",
                        format_args!(",\"addr\":\"{}\"", json_escape(&addr.to_string())),
                    );
                }

                if config.drop_every != 0 && packets_total % config.drop_every == 0 {
                    packets_dropped = packets_dropped.saturating_add(1);
                    if config.verbose_chunks {
                        logger.event(
                            "udp_server",
                            "simulated_drop",
                            format_args!(",\"packet_index\":{},\"bytes\":{}", packets_total, len),
                        );
                    }
                    continue;
                }

                let (header, payload) = match decode_udp_packet(&buf[..len]) {
                    Ok(packet) => packet,
                    Err(err) => {
                        logger.event(
                            "udp_server",
                            "bad_packet",
                            format_args!(
                                ",\"peer\":\"{}\",\"bytes\":{},\"message\":\"{}\"",
                                json_escape(&addr.to_string()),
                                len,
                                json_escape(&err.to_string())
                            ),
                        );
                        continue;
                    }
                };

                if header.packet_type == UDP_PACKET_BYE {
                    frames_expired +=
                        expire_udp_frames(&logger, "bye", &mut frames, Duration::from_millis(0));
                    if let Some(addr) = peer {
                        if send_udp_status(
                            &socket,
                            addr,
                            active_session_id.unwrap_or(header.session_id),
                            last_observed_frame_id,
                            frames_complete,
                            frames_expired,
                            frames.len() as u32,
                            chunks_total,
                            announce_packets_total,
                        ) {
                            status_packets_sent = status_packets_sent.saturating_add(1);
                        }
                    }
                    logger.event(
                        "udp_server",
                        "summary",
                        format_args!(
                            ",\"reason\":\"bye\",\"packets_total\":{},\"packets_dropped\":{},\"announce_packets_total\":{},\"chunks_total\":{},\"bytes_total\":{},\"frames_complete\":{},\"frames_expired\":{},\"open_frames\":{},\"synthetic_frames_opened\":{},\"nack_packets_sent\":{},\"nack_chunks_requested\":{},\"status_packets_sent\":{}",
                            packets_total,
                            packets_dropped,
                            announce_packets_total,
                            chunks_total,
                            bytes_total,
                            frames_complete,
                            frames_expired,
                            frames.len(),
                            synthetic_frames_opened,
                            nack_packets_sent,
                            nack_chunks_requested,
                            status_packets_sent
                        ),
                    );
                    return Ok(());
                }

                if header.packet_type == UDP_PACKET_NACK {
                    continue;
                }

                if header.packet_type == UDP_PACKET_VIDEO
                    || header.packet_type == UDP_PACKET_ANNOUNCE
                {
                    match active_session_id {
                        Some(session_id) if session_id == header.session_id => {
                            if peer != Some(addr) {
                                peer = Some(addr);
                                logger.event(
                                    "udp_server",
                                    "peer_update",
                                    format_args!(
                                        ",\"addr\":\"{}\",\"session_id\":{}",
                                        json_escape(&addr.to_string()),
                                        header.session_id
                                    ),
                                );
                            }
                        }
                        _ => {
                            if active_session_id.is_some() {
                                frames_expired += expire_udp_frames(
                                    &logger,
                                    "session_replaced",
                                    &mut frames,
                                    Duration::from_millis(0),
                                );
                                completed_frames.clear();
                                initial_frame_video_drop_counts.clear();
                                last_observed_frame_id = None;
                            }
                            active_session_id = Some(header.session_id);
                            peer = Some(addr);
                            logger.event(
                                "udp_server",
                                "session",
                                format_args!(
                                    ",\"addr\":\"{}\",\"session_id\":{}",
                                    json_escape(&addr.to_string()),
                                    header.session_id
                                ),
                            );
                        }
                    }
                }

                if should_drop_initial_frame_video(
                    &config,
                    &header,
                    &mut initial_frame_video_drop_counts,
                ) {
                    packets_dropped = packets_dropped.saturating_add(1);
                    if config.verbose_chunks {
                        logger.event(
                            "udp_server",
                            "simulated_initial_frame_video_drop",
                            format_args!(
                                ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{}",
                                header.frame_id, header.chunk_index, header.chunk_count
                            ),
                        );
                    }
                    continue;
                }

                if header.chunk_count == 0 || header.chunk_index >= header.chunk_count {
                    logger.event(
                        "udp_server",
                        "bad_chunk",
                        format_args!(
                            ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{}",
                            header.frame_id, header.chunk_index, header.chunk_count
                        ),
                    );
                    continue;
                }

                if let Some(last_frame_id) = last_observed_frame_id {
                    synthetic_frames_opened =
                        synthetic_frames_opened.saturating_add(open_udp_frame_gaps(
                            &logger,
                            &mut frames,
                            &completed_frames,
                            &header,
                            last_frame_id,
                            config.quiet_frames,
                        ));
                }
                match last_observed_frame_id {
                    Some(last_frame_id) if header.frame_id <= last_frame_id => {}
                    _ => last_observed_frame_id = Some(header.frame_id),
                }

                if header.packet_type == UDP_PACKET_ANNOUNCE {
                    announce_packets_total = announce_packets_total.saturating_add(1);
                    open_udp_frame_state(
                        &logger,
                        "frame_announced",
                        &mut frames,
                        &completed_frames,
                        &header,
                        None,
                        config.quiet_frames,
                    );
                    if let Some(addr) = peer {
                        let (nack_packets, nack_chunks) = send_udp_nacks(
                            &socket,
                            addr,
                            &logger,
                            &mut frames,
                            &mut nack_buf,
                            &config,
                        );
                        nack_packets_sent = nack_packets_sent.saturating_add(nack_packets);
                        nack_chunks_requested = nack_chunks_requested.saturating_add(nack_chunks);
                    }
                    frames_expired +=
                        expire_udp_frames(&logger, "frame_timeout", &mut frames, frame_timeout);
                    if let Some(addr) = peer {
                        if should_send_udp_status(config.status_interval_ms, &mut last_status_at)
                            && send_udp_status(
                                &socket,
                                addr,
                                active_session_id.unwrap_or(header.session_id),
                                last_observed_frame_id,
                                frames_complete,
                                frames_expired,
                                frames.len() as u32,
                                chunks_total,
                                announce_packets_total,
                            )
                        {
                            status_packets_sent = status_packets_sent.saturating_add(1);
                        }
                    }
                    continue;
                }

                chunks_total = chunks_total.saturating_add(1);
                bytes_total = bytes_total.saturating_add(u64::from(header.payload_len));
                if completed_frames.contains_key(&header.frame_id) {
                    if config.verbose_chunks {
                        logger.event(
                            "udp_server",
                            "late_complete_chunk",
                            format_args!(
                                ",\"frame_id\":{},\"chunk_index\":{}",
                                header.frame_id, header.chunk_index
                            ),
                        );
                    }
                    continue;
                }
                if config.verbose_chunks {
                    logger.event(
                        "udp_server",
                        "chunk",
                        format_args!(
                            ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{},\"payload_len\":{}",
                            header.frame_id,
                            header.chunk_index,
                            header.chunk_count,
                            header.payload_len
                        ),
                    );
                }

                open_udp_frame_state(
                    &logger,
                    "frame_rx_start",
                    &mut frames,
                    &completed_frames,
                    &header,
                    Some(header.chunk_index),
                    config.quiet_frames,
                );

                let mut complete = false;
                if let Some(frame) = frames.get_mut(&header.frame_id) {
                    if frame.chunk_count != header.chunk_count
                        || frame.frame_size != header.frame_size
                    {
                        logger.event(
                            "udp_server",
                            "frame_mismatch",
                            format_args!(
                                ",\"frame_id\":{},\"expected_chunks\":{},\"got_chunks\":{},\"expected_size\":{},\"got_size\":{}",
                                header.frame_id,
                                frame.chunk_count,
                                header.chunk_count,
                                frame.frame_size,
                                header.frame_size
                            ),
                        );
                        continue;
                    }
                    let index = header.chunk_index as usize;
                    if frame.seen[index] {
                        logger.event(
                            "udp_server",
                            "duplicate_chunk",
                            format_args!(
                                ",\"frame_id\":{},\"chunk_index\":{}",
                                header.frame_id, header.chunk_index
                            ),
                        );
                    } else {
                        frame.seen[index] = true;
                        frame.chunks_seen += 1;
                        frame.bytes_seen = frame.bytes_seen.saturating_add(header.payload_len);
                    }
                    if frame.chunks_seen == frame.chunk_count {
                        complete = true;
                        frames_complete = frames_complete.saturating_add(1);
                        completed_frames.insert(header.frame_id, Instant::now());
                        prune_udp_completed_frames(&mut completed_frames, completed_retention);
                        if !config.quiet_frames {
                            logger.event(
                                "udp_server",
                                "frame_complete",
                                format_args!(
                                    ",\"frame_id\":{},\"chunks_seen\":{},\"chunk_count\":{},\"bytes_seen\":{},\"sender_first_unix_us\":{},\"elapsed_us\":{}",
                                    header.frame_id,
                                    frame.chunks_seen,
                                    frame.chunk_count,
                                    frame.bytes_seen,
                                    frame.first_unix_us,
                                    frame.started.elapsed().as_micros()
                                ),
                            );
                        }
                    }
                }
                if complete {
                    frames.remove(&header.frame_id);
                }
                if let Some(addr) = peer {
                    let (nack_packets, nack_chunks) =
                        send_udp_nacks(&socket, addr, &logger, &mut frames, &mut nack_buf, &config);
                    nack_packets_sent = nack_packets_sent.saturating_add(nack_packets);
                    nack_chunks_requested = nack_chunks_requested.saturating_add(nack_chunks);
                }
                frames_expired +=
                    expire_udp_frames(&logger, "frame_timeout", &mut frames, frame_timeout);
                if let Some(addr) = peer {
                    if should_send_udp_status(config.status_interval_ms, &mut last_status_at)
                        && send_udp_status(
                            &socket,
                            addr,
                            active_session_id.unwrap_or(header.session_id),
                            last_observed_frame_id,
                            frames_complete,
                            frames_expired,
                            frames.len() as u32,
                            chunks_total,
                            announce_packets_total,
                        )
                    {
                        status_packets_sent = status_packets_sent.saturating_add(1);
                    }
                }
                let _ = payload;
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                if let Some(addr) = peer {
                    let (nack_packets, nack_chunks) =
                        send_udp_nacks(&socket, addr, &logger, &mut frames, &mut nack_buf, &config);
                    nack_packets_sent = nack_packets_sent.saturating_add(nack_packets);
                    nack_chunks_requested = nack_chunks_requested.saturating_add(nack_chunks);
                }
                frames_expired +=
                    expire_udp_frames(&logger, "frame_timeout", &mut frames, frame_timeout);
                if let (Some(addr), Some(session_id)) = (peer, active_session_id) {
                    if should_send_udp_status(config.status_interval_ms, &mut last_status_at)
                        && send_udp_status(
                            &socket,
                            addr,
                            session_id,
                            last_observed_frame_id,
                            frames_complete,
                            frames_expired,
                            frames.len() as u32,
                            chunks_total,
                            announce_packets_total,
                        )
                    {
                        status_packets_sent = status_packets_sent.saturating_add(1);
                    }
                }
                if config.read_timeout_ms != 0
                    && last_packet_at.elapsed() >= Duration::from_millis(config.read_timeout_ms)
                {
                    frames_expired += expire_udp_frames(
                        &logger,
                        "read_timeout",
                        &mut frames,
                        Duration::from_millis(0),
                    );
                    logger.event(
                        "udp_server",
                        "summary",
                        format_args!(
                            ",\"reason\":\"read_timeout\",\"packets_total\":{},\"packets_dropped\":{},\"announce_packets_total\":{},\"chunks_total\":{},\"bytes_total\":{},\"frames_complete\":{},\"frames_expired\":{},\"open_frames\":{},\"synthetic_frames_opened\":{},\"nack_packets_sent\":{},\"nack_chunks_requested\":{},\"status_packets_sent\":{}",
                            packets_total,
                            packets_dropped,
                            announce_packets_total,
                            chunks_total,
                            bytes_total,
                            frames_complete,
                            frames_expired,
                            frames.len(),
                            synthetic_frames_opened,
                            nack_packets_sent,
                            nack_chunks_requested,
                            status_packets_sent
                        ),
                    );
                    return Ok(());
                }
            }
            Err(err) => {
                logger.event(
                    "udp_server",
                    "fatal",
                    format_args!(",\"message\":\"{}\"", json_escape(&err.to_string())),
                );
                return Err(format!("UDP server read failed: {err}"));
            }
        }
    }
}

fn run_udp_client(config: UdpClientConfig) -> Result<(), String> {
    validate_udp_client_config(&config)?;
    let logger = Arc::new(Logger::new(config.log_path.as_deref())?);
    let socket = UdpSocket::bind(&config.bind)
        .map_err(|err| format!("failed to bind UDP '{}': {err}", config.bind))?;
    socket
        .connect(&config.connect)
        .map_err(|err| format!("failed to connect UDP '{}': {err}", config.connect))?;
    socket
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure UDP nonblocking mode: {err}"))?;

    let session_id = unix_us();
    logger.event(
        "udp_client",
        "connect",
        format_args!(
            ",\"peer\":\"{}\",\"bind\":\"{}\",\"session_id\":{},\"duration_sec\":{},\"fps\":{},\"frame_size\":{},\"payload_size\":{},\"pace_every\":{},\"pace_us\":{},\"resend_cache_frames\":{},\"nack_linger_ms\":{},\"announce_frames\":{}",
            json_escape(&config.connect),
            json_escape(&config.bind),
            session_id,
            config.duration_sec,
            config.fps,
            config.frame_size,
            config.payload_size,
            config.pace_every,
            config.pace_us,
            config.resend_cache_frames,
            config.nack_linger_ms,
            config.announce_frames
        ),
    );

    let frame_count_target = config.duration_sec.saturating_mul(u64::from(config.fps));
    let frame_period = Duration::from_nanos(1_000_000_000_u64 / u64::from(config.fps));
    let chunk_count = div_ceil_u32(config.frame_size, config.payload_size);
    let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + config.payload_size as usize];
    let mut rx_buf = vec![0_u8; MAX_UDP_PACKET_LEN];
    let mut sent_cache: BTreeMap<u64, SentUdpFrame> = BTreeMap::new();
    let mut stats = UdpClientStats::default();
    let mut frames_sent = 0_u64;
    let mut packets_sent = 0_u64;
    let mut bytes_sent = 0_u64;
    let started = Instant::now();
    let mut next_frame = Instant::now();

    while frames_sent < frame_count_target {
        let frame_started = Instant::now();
        if !config.quiet_frames {
            logger.event(
                "udp_client",
                "frame_send_start",
                format_args!(
                    ",\"frame_id\":{},\"chunk_count\":{},\"frame_size\":{}",
                    frames_sent, chunk_count, config.frame_size
                ),
            );
        }

        if config.announce_frames {
            let announce = UdpVideoPacketHeader {
                packet_type: UDP_PACKET_ANNOUNCE,
                session_id,
                frame_id: frames_sent,
                chunk_index: 0,
                chunk_count,
                frame_size: config.frame_size,
                payload_len: 0,
                sent_unix_us: unix_us(),
            };
            encode_udp_packet_header(&announce, &mut packet[..UDP_PACKET_HEADER_LEN]);
            send_udp_datagram(&socket, &packet[..UDP_PACKET_HEADER_LEN], "announce")?;
            stats.announce_packets_sent = stats.announce_packets_sent.saturating_add(1);
        }

        if config.resend_cache_frames != 0 {
            sent_cache.insert(
                frames_sent,
                SentUdpFrame {
                    chunks: Vec::with_capacity(chunk_count as usize),
                },
            );
        }

        let mut offset = 0_u32;
        for chunk_index in 0..chunk_count {
            let remaining = config.frame_size - offset;
            let payload_len = remaining.min(config.payload_size);
            fill_payload(
                &mut packet[UDP_PACKET_HEADER_LEN..UDP_PACKET_HEADER_LEN + payload_len as usize],
                frames_sent,
                chunk_index,
            );
            let header = UdpVideoPacketHeader {
                packet_type: UDP_PACKET_VIDEO,
                session_id,
                frame_id: frames_sent,
                chunk_index,
                chunk_count,
                frame_size: config.frame_size,
                payload_len,
                sent_unix_us: unix_us(),
            };
            encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
            let packet_len = UDP_PACKET_HEADER_LEN + payload_len as usize;
            send_udp_datagram(&socket, &packet[..packet_len], "video")?;
            if config.resend_cache_frames != 0 {
                if let Some(frame_cache) = sent_cache.get_mut(&frames_sent) {
                    frame_cache.chunks.push(packet[..packet_len].to_vec());
                }
            }
            packets_sent = packets_sent.saturating_add(1);
            bytes_sent = bytes_sent.saturating_add(u64::from(payload_len));
            offset += payload_len;

            if config.verbose_chunks {
                logger.event(
                    "udp_client",
                    "chunk_sent",
                    format_args!(
                        ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{},\"payload_len\":{}",
                        frames_sent, chunk_index, chunk_count, payload_len
                    ),
                );
            }
            maybe_pace_udp(
                config.pace_every,
                config.pace_us,
                chunk_index + 1,
                chunk_count,
            );
            drain_udp_nacks(
                &socket,
                &logger,
                &mut rx_buf,
                &sent_cache,
                session_id,
                &mut stats,
                config.verbose_chunks,
            )?;
        }

        if config.resend_cache_frames != 0 {
            prune_udp_sent_cache(&mut sent_cache, config.resend_cache_frames);
        }
        drain_udp_nacks(
            &socket,
            &logger,
            &mut rx_buf,
            &sent_cache,
            session_id,
            &mut stats,
            config.verbose_chunks,
        )?;

        if !config.quiet_frames {
            logger.event(
                "udp_client",
                "frame_sent",
                format_args!(
                    ",\"frame_id\":{},\"chunks_sent\":{},\"bytes_sent\":{},\"elapsed_us\":{}",
                    frames_sent,
                    chunk_count,
                    config.frame_size,
                    frame_started.elapsed().as_micros()
                ),
            );
        }
        frames_sent += 1;
        next_frame += frame_period;
        if let Some(sleep_for) = next_frame.checked_duration_since(Instant::now()) {
            thread::sleep(sleep_for);
        } else {
            logger.event(
                "udp_client",
                "schedule_late",
                format_args!(
                    ",\"frame_id\":{},\"elapsed_ms\":{}",
                    frames_sent,
                    started.elapsed().as_millis()
                ),
            );
            next_frame = Instant::now();
        }
    }

    let linger_deadline = Instant::now() + Duration::from_millis(config.nack_linger_ms);
    while Instant::now() < linger_deadline {
        drain_udp_nacks(
            &socket,
            &logger,
            &mut rx_buf,
            &sent_cache,
            session_id,
            &mut stats,
            config.verbose_chunks,
        )?;
        thread::sleep(Duration::from_millis(1));
    }

    let bye = UdpVideoPacketHeader {
        packet_type: UDP_PACKET_BYE,
        session_id,
        frame_id: frames_sent,
        chunk_index: 0,
        chunk_count: 0,
        frame_size: 0,
        payload_len: 0,
        sent_unix_us: unix_us(),
    };
    encode_udp_packet_header(&bye, &mut packet[..UDP_PACKET_HEADER_LEN]);
    for bye_index in 0..3 {
        if let Err(err) = send_udp_datagram(&socket, &packet[..UDP_PACKET_HEADER_LEN], "bye") {
            logger.event(
                "udp_client",
                "bye_send_error",
                format_args!(",\"bye_index\":{},\"error\":\"{}\"", bye_index, err),
            );
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    logger.event(
        "udp_client",
        "summary",
        format_args!(
            ",\"frames_sent\":{},\"packets_sent\":{},\"announce_packets_sent\":{},\"bytes_sent\":{},\"elapsed_ms\":{},\"nacks_received\":{},\"nack_chunks_requested\":{},\"retransmit_packets\":{},\"retransmit_bytes\":{},\"retransmit_misses\":{},\"statuses_received\":{},\"status_last_observed_frame_id\":{},\"status_frames_complete\":{},\"status_frames_skipped\":{}",
            frames_sent,
            packets_sent,
            stats.announce_packets_sent,
            bytes_sent,
            started.elapsed().as_millis(),
            stats.nacks_received,
            stats.nack_chunks_requested,
            stats.retransmit_packets,
            stats.retransmit_bytes,
            stats.retransmit_misses,
            stats.statuses_received,
            stats.status_last_observed_frame_id,
            stats.status_frames_complete,
            stats.status_frames_skipped
        ),
    );
    Ok(())
}

fn run_client(config: ClientConfig) -> Result<(), String> {
    validate_client_config(&config)?;
    let logger = Arc::new(Logger::new(config.log_path.as_deref())?);
    let mut stream = TcpStream::connect(&config.connect)
        .map_err(|err| format!("failed to connect '{}': {err}", config.connect))?;
    stream
        .set_nodelay(config.tcp_nodelay)
        .map_err(|err| format!("failed to configure TCP_NODELAY: {err}"))?;
    if config.io_timeout_ms != 0 {
        let timeout = Some(Duration::from_millis(config.io_timeout_ms));
        stream
            .set_write_timeout(timeout)
            .map_err(|err| format!("failed to configure write timeout: {err}"))?;
        stream
            .set_read_timeout(timeout)
            .map_err(|err| format!("failed to configure read timeout: {err}"))?;
    }

    logger.event(
        "client",
        "connect",
        format_args!(
            ",\"peer\":\"{}\",\"mode\":\"{}\",\"duration_sec\":{},\"fps\":{},\"frame_size\":{},\"chunk_size\":{},\"window_bytes\":{},\"window_wait_ms\":{},\"drop_when_window_full\":{},\"pace_every\":{},\"pace_us\":{},\"io_timeout_ms\":{},\"tcp_nodelay\":{}",
            json_escape(&config.connect),
            config.mode.as_str(),
            config.duration_sec,
            config.fps,
            config.frame_size,
            config.chunk_size,
            config.window_bytes,
            config.window_wait_ms,
            config.drop_when_window_full,
            config.pace_every,
            config.pace_us,
            config.io_timeout_ms,
            config.tcp_nodelay
        ),
    );

    let flow = Arc::new((Mutex::new(FlowState::default()), Condvar::new()));
    let ack_stream = stream
        .try_clone()
        .map_err(|err| format!("failed to clone TCP stream: {err}"))?;
    let ack_logger = Arc::clone(&logger);
    let ack_flow = Arc::clone(&flow);
    let ack_thread = thread::spawn(move || client_ack_loop(ack_stream, ack_logger, ack_flow));

    let hello = HelloMsg {
        chunk_size: config.chunk_size,
        frame_size: config.frame_size,
        fps: config.fps,
        duration_sec: config.duration_sec.min(u64::from(u32::MAX)) as u32,
        window_bytes: config.window_bytes.min(u64::from(u32::MAX)) as u32,
        mode: config.mode.wire(),
    };
    if let Err(err) = write_hello(&mut stream, &hello) {
        logger.event(
            "client",
            "hello_write_error",
            format_args!(",\"message\":\"{}\"", json_escape(&err.to_string())),
        );
        return Err(format!("hello write failed: {err}"));
    }

    let frame_count_target = config.duration_sec.saturating_mul(u64::from(config.fps));
    let frame_period = Duration::from_nanos(1_000_000_000_u64 / u64::from(config.fps));
    let mut next_frame = Instant::now();
    let started = Instant::now();
    let mut payload = vec![0_u8; config.chunk_size as usize];
    let mut frames_sent = 0_u64;
    let mut frames_skipped = 0_u64;
    let mut chunks_sent = 0_u64;
    let mut bytes_sent = 0_u64;

    while frames_sent < frame_count_target {
        let frame_started = Instant::now();
        let chunk_count = div_ceil_u32(config.frame_size, config.chunk_size);
        if config.drop_when_window_full
            && !reserve_window_capacity(
                &config,
                &flow,
                &logger,
                frames_sent,
                u64::from(config.frame_size),
                true,
            )?
        {
            frames_skipped = frames_skipped.saturating_add(1);
            logger.event(
                "client",
                "frame_skipped_window_full",
                format_args!(
                    ",\"frame_id\":{},\"frame_size\":{},\"window_bytes\":{},\"window_wait_ms\":{}",
                    frames_sent, config.frame_size, config.window_bytes, config.window_wait_ms
                ),
            );
            frames_sent += 1;
            next_frame += frame_period;
            if let Some(sleep_for) = next_frame.checked_duration_since(Instant::now()) {
                thread::sleep(sleep_for);
            } else {
                logger.event(
                    "client",
                    "schedule_late",
                    format_args!(
                        ",\"frame_id\":{},\"elapsed_ms\":{}",
                        frames_sent,
                        started.elapsed().as_millis()
                    ),
                );
                next_frame = Instant::now();
            }
            continue;
        }

        logger.event(
            "client",
            "frame_send_start",
            format_args!(
                ",\"frame_id\":{},\"chunk_count\":{},\"frame_size\":{}",
                frames_sent, chunk_count, config.frame_size
            ),
        );

        let mut offset = 0_u32;
        for chunk_index in 0..chunk_count {
            let remaining = config.frame_size - offset;
            let payload_len = remaining.min(config.chunk_size);
            fill_payload(
                &mut payload[..payload_len as usize],
                frames_sent,
                chunk_index,
            );
            let chunk = ChunkMsg {
                frame_id: frames_sent,
                chunk_index,
                chunk_count,
                frame_size: config.frame_size,
                sent_unix_us: unix_us(),
                payload_len,
            };
            if !config.drop_when_window_full {
                reserve_window(&config, &flow, &logger, frames_sent, payload_len)?;
            }
            if let Err(err) = write_chunk(&mut stream, &chunk, &payload[..payload_len as usize]) {
                logger.event(
                    "client",
                    "chunk_write_error",
                    format_args!(
                        ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{},\"message\":\"{}\"",
                        frames_sent,
                        chunk_index,
                        chunk_count,
                        json_escape(&err.to_string())
                    ),
                );
                return Err(format!("chunk write failed: {err}"));
            }
            chunks_sent += 1;
            bytes_sent += u64::from(payload_len);
            offset += payload_len;

            if config.verbose_chunks {
                logger.event(
                    "client",
                    "chunk_sent",
                    format_args!(
                        ",\"frame_id\":{},\"chunk_index\":{},\"chunk_count\":{},\"payload_len\":{}",
                        frames_sent, chunk_index, chunk_count, payload_len
                    ),
                );
            }

            maybe_pace(&config, chunk_index + 1, chunk_count);
        }

        logger.event(
            "client",
            "frame_sent",
            format_args!(
                ",\"frame_id\":{},\"chunks_sent\":{},\"bytes_sent\":{},\"elapsed_us\":{}",
                frames_sent,
                chunk_count,
                config.frame_size,
                frame_started.elapsed().as_micros()
            ),
        );
        frames_sent += 1;
        next_frame += frame_period;
        if let Some(sleep_for) = next_frame.checked_duration_since(Instant::now()) {
            thread::sleep(sleep_for);
        } else {
            logger.event(
                "client",
                "schedule_late",
                format_args!(
                    ",\"frame_id\":{},\"elapsed_ms\":{}",
                    frames_sent,
                    started.elapsed().as_millis()
                ),
            );
            next_frame = Instant::now();
        }
    }

    if let Err(err) = write_bye(&mut stream) {
        logger.event(
            "client",
            "bye_write_error",
            format_args!(",\"message\":\"{}\"", json_escape(&err.to_string())),
        );
        return Err(format!("bye write failed: {err}"));
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = ack_thread.join();
    let state = flow
        .0
        .lock()
        .map_err(|_| "flow state lock poisoned".to_string())?;
    logger.event(
        "client",
        "summary",
        format_args!(
            ",\"frames_sent\":{},\"frames_skipped\":{},\"chunks_sent\":{},\"bytes_sent\":{},\"acked_frames\":{},\"acked_bytes\":{},\"inflight_bytes\":{}",
            frames_sent,
            frames_skipped,
            chunks_sent,
            bytes_sent,
            state.acked_frames,
            state.acked_bytes,
            state.inflight_bytes
        ),
    );
    Ok(())
}

fn client_ack_loop(
    mut stream: TcpStream,
    logger: Arc<Logger>,
    flow: Arc<(Mutex<FlowState>, Condvar)>,
) {
    let mut scratch = Vec::with_capacity(1024);
    loop {
        match read_wire_message(&mut stream, &mut scratch) {
            Ok(Some(WireMessage::FrameAck(ack))) => {
                let acked_bytes_for_frame = u64::from(ack.bytes_seen);
                let mut newly_acked_bytes = 0_u64;
                let mut inflight_after = 0_u64;
                let mut acked_frames_after = 0_u64;
                let mut acked_bytes_after = 0_u64;
                let (lock, cvar) = &*flow;
                if let Ok(mut state) = lock.lock() {
                    let previous = state
                        .acked_frame_bytes
                        .get(&ack.frame_id)
                        .copied()
                        .unwrap_or(0);
                    if acked_bytes_for_frame > previous {
                        newly_acked_bytes = acked_bytes_for_frame - previous;
                        state.inflight_bytes =
                            state.inflight_bytes.saturating_sub(newly_acked_bytes);
                        state.acked_bytes = state.acked_bytes.saturating_add(newly_acked_bytes);
                        if ack.complete {
                            state.acked_frame_bytes.remove(&ack.frame_id);
                        } else {
                            state
                                .acked_frame_bytes
                                .insert(ack.frame_id, acked_bytes_for_frame);
                        }
                    }

                    if ack.complete {
                        state.acked_frames += 1;
                        state.acked_frame_bytes.remove(&ack.frame_id);
                    }

                    inflight_after = state.inflight_bytes;
                    acked_frames_after = state.acked_frames;
                    acked_bytes_after = state.acked_bytes;
                    cvar.notify_all();
                }

                logger.event(
                    "client",
                    "frame_ack",
                    format_args!(
                        ",\"frame_id\":{},\"complete\":{},\"chunks_seen\":{},\"chunk_count\":{},\"frame_size\":{},\"bytes_seen\":{},\"newly_acked_bytes\":{},\"inflight_bytes\":{},\"acked_frames\":{},\"acked_bytes\":{},\"receive_elapsed_us\":{}",
                        ack.frame_id,
                        ack.complete,
                        ack.chunks_seen,
                        ack.chunk_count,
                        ack.frame_size,
                        ack.bytes_seen,
                        newly_acked_bytes,
                        inflight_after,
                        acked_frames_after,
                        acked_bytes_after,
                        ack.receive_elapsed_us
                    ),
                );
            }
            Ok(Some(WireMessage::Bye)) | Ok(None) => {
                logger.event("client", "ack_eof", format_args!(""));
                return;
            }
            Ok(Some(other)) => {
                logger.event(
                    "client",
                    "unexpected_message",
                    format_args!(",\"message\":\"{:?}\"", json_escape(&format!("{other:?}"))),
                );
            }
            Err(err) => {
                logger.event(
                    "client",
                    "ack_error",
                    format_args!(",\"message\":\"{}\"", json_escape(&err.to_string())),
                );
                return;
            }
        }
    }
}

fn reserve_window(
    config: &ClientConfig,
    flow: &Arc<(Mutex<FlowState>, Condvar)>,
    logger: &Logger,
    frame_id: u64,
    payload_len: u32,
) -> Result<(), String> {
    reserve_window_capacity(
        config,
        flow,
        logger,
        frame_id,
        u64::from(payload_len),
        false,
    )
    .map(|_| ())
}

fn reserve_window_capacity(
    config: &ClientConfig,
    flow: &Arc<(Mutex<FlowState>, Condvar)>,
    logger: &Logger,
    frame_id: u64,
    bytes: u64,
    allow_drop: bool,
) -> Result<bool, String> {
    if config.mode != SendMode::Window {
        return Ok(true);
    }
    let (lock, cvar) = &**flow;
    let mut state = lock
        .lock()
        .map_err(|_| "flow state lock poisoned".to_string())?;
    let timeout = Duration::from_millis(config.window_wait_ms);
    let wait_started = Instant::now();
    while state.inflight_bytes.saturating_add(bytes) > config.window_bytes {
        if allow_drop && config.window_wait_ms == 0 {
            return Ok(false);
        }
        let wait_for = if config.window_wait_ms == 0 {
            Duration::from_millis(200)
        } else {
            let remaining = timeout.saturating_sub(wait_started.elapsed());
            if remaining.is_zero() {
                if allow_drop {
                    return Ok(false);
                }
                logger.event(
                    "client",
                    "window_timeout",
                    format_args!(
                        ",\"frame_id\":{},\"inflight_bytes\":{},\"window_bytes\":{},\"required_bytes\":{},\"window_wait_ms\":{}",
                        frame_id, state.inflight_bytes, config.window_bytes, bytes, config.window_wait_ms
                    ),
                );
                return Err("window mode timed out waiting for frame ACK".to_string());
            }
            remaining.min(Duration::from_millis(200))
        };
        let (next_state, wait_result) = cvar
            .wait_timeout(state, wait_for)
            .map_err(|_| "flow state lock poisoned".to_string())?;
        state = next_state;
        if config.window_wait_ms != 0
            && wait_result.timed_out()
            && wait_started.elapsed() >= timeout
        {
            if allow_drop {
                return Ok(false);
            }
            logger.event(
                "client",
                "window_timeout",
                format_args!(
                    ",\"frame_id\":{},\"inflight_bytes\":{},\"window_bytes\":{},\"required_bytes\":{},\"window_wait_ms\":{}",
                    frame_id, state.inflight_bytes, config.window_bytes, bytes, config.window_wait_ms
                ),
            );
            return Err("window mode timed out waiting for frame ACK".to_string());
        }
    }
    state.inflight_bytes = state.inflight_bytes.saturating_add(bytes);
    Ok(true)
}

fn maybe_pause_server_read(
    logger: &Logger,
    config: &ServerConfig,
    started: Instant,
    next_pause_after_ms: &mut Option<u64>,
    pauses_taken: &mut u64,
) {
    let Some(pause_after_ms) = *next_pause_after_ms else {
        return;
    };
    if started.elapsed() < Duration::from_millis(pause_after_ms) {
        return;
    }

    logger.event(
        "server",
        "read_pause_start",
        format_args!(
            ",\"pause_index\":{},\"elapsed_ms\":{},\"duration_ms\":{}",
            *pauses_taken,
            started.elapsed().as_millis(),
            config.pause_read_duration_ms
        ),
    );
    thread::sleep(Duration::from_millis(config.pause_read_duration_ms));
    *pauses_taken = (*pauses_taken).saturating_add(1);
    logger.event(
        "server",
        "read_pause_end",
        format_args!(
            ",\"pause_index\":{},\"elapsed_ms\":{}",
            (*pauses_taken).saturating_sub(1),
            started.elapsed().as_millis()
        ),
    );

    if config.pause_read_repeat_ms == 0 {
        *next_pause_after_ms = None;
        return;
    }

    let mut next = pause_after_ms.saturating_add(config.pause_read_repeat_ms);
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    while next <= elapsed_ms {
        next = next.saturating_add(config.pause_read_repeat_ms);
        if next == u64::MAX {
            break;
        }
    }
    *next_pause_after_ms = Some(next);
}

fn maybe_pace(config: &ClientConfig, chunks_sent_in_frame: u32, chunk_count: u32) {
    if config.mode == SendMode::Burst || config.pace_every == 0 || config.pace_us == 0 {
        return;
    }
    if chunks_sent_in_frame >= chunk_count {
        return;
    }
    if chunks_sent_in_frame % config.pace_every == 0 {
        thread::sleep(Duration::from_micros(config.pace_us));
    }
}

fn maybe_pace_udp(pace_every: u32, pace_us: u64, chunks_sent_in_frame: u32, chunk_count: u32) {
    if pace_every == 0 || pace_us == 0 {
        return;
    }
    if chunks_sent_in_frame >= chunk_count {
        return;
    }
    if chunks_sent_in_frame % pace_every == 0 {
        thread::sleep(Duration::from_micros(pace_us));
    }
}

fn prune_udp_sent_cache(cache: &mut BTreeMap<u64, SentUdpFrame>, max_frames: usize) {
    while cache.len() > max_frames {
        let Some(frame_id) = cache.keys().next().copied() else {
            break;
        };
        cache.remove(&frame_id);
    }
}

fn should_drop_initial_frame_video(
    config: &UdpServerConfig,
    header: &UdpVideoPacketHeader,
    drop_counts: &mut BTreeMap<u64, u32>,
) -> bool {
    if config.drop_initial_frame_video_every == 0 || header.packet_type != UDP_PACKET_VIDEO {
        return false;
    }
    if header.frame_id % config.drop_initial_frame_video_every != 0 {
        return false;
    }
    let dropped = drop_counts.entry(header.frame_id).or_insert(0);
    if *dropped >= header.chunk_count {
        return false;
    }
    *dropped += 1;
    true
}

fn send_udp_datagram(socket: &UdpSocket, packet: &[u8], context: &str) -> Result<usize, String> {
    let started = Instant::now();
    let timeout = Duration::from_millis(500);
    loop {
        match socket.send(packet) {
            Ok(len) => return Ok(len),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err(format!("UDP {context} send timed out after would-block"));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) => return Err(format!("UDP {context} send failed: {err}")),
        }
    }
}

fn should_send_udp_status(interval_ms: u64, last_status_at: &mut Instant) -> bool {
    if interval_ms == 0 {
        return false;
    }
    if last_status_at.elapsed() < Duration::from_millis(interval_ms) {
        return false;
    }
    *last_status_at = Instant::now();
    true
}

fn send_udp_status(
    socket: &UdpSocket,
    peer: SocketAddr,
    session_id: u64,
    last_observed_frame_id: Option<u64>,
    frames_complete: u64,
    frames_skipped: u64,
    open_frames: u32,
    chunks_total: u64,
    announce_packets_total: u64,
) -> bool {
    let status = UdpStatusMsg {
        last_observed_frame_id: last_observed_frame_id.unwrap_or(u64::MAX),
        frames_complete,
        frames_skipped,
        open_frames,
        chunks_total,
        announce_packets_total,
    };
    let header = UdpVideoPacketHeader {
        packet_type: UDP_PACKET_STATUS,
        session_id,
        frame_id: status.last_observed_frame_id,
        chunk_index: 0,
        chunk_count: 0,
        frame_size: 0,
        payload_len: UDP_STATUS_BODY_LEN as u32,
        sent_unix_us: unix_us(),
    };
    let mut packet = [0_u8; UDP_PACKET_HEADER_LEN + UDP_STATUS_BODY_LEN];
    encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
    encode_udp_status(&status, &mut packet[UDP_PACKET_HEADER_LEN..]);
    socket.send_to(&packet, peer).is_ok()
}

fn drain_udp_nacks(
    socket: &UdpSocket,
    logger: &Logger,
    rx_buf: &mut [u8],
    sent_cache: &BTreeMap<u64, SentUdpFrame>,
    session_id: u64,
    stats: &mut UdpClientStats,
    verbose: bool,
) -> Result<(), String> {
    loop {
        let len = match socket.recv(rx_buf) {
            Ok(len) => len,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(format!("UDP control receive failed: {err}")),
        };

        let (header, payload) = match decode_udp_packet(&rx_buf[..len]) {
            Ok(packet) => packet,
            Err(err) => {
                logger.event(
                    "udp_client",
                    "bad_control_packet",
                    format_args!(
                        ",\"bytes\":{},\"message\":\"{}\"",
                        len,
                        json_escape(&err.to_string())
                    ),
                );
                continue;
            }
        };
        if header.packet_type == UDP_PACKET_STATUS {
            let status = match decode_udp_status(payload) {
                Ok(status) => status,
                Err(err) => {
                    logger.event(
                        "udp_client",
                        "bad_status_packet",
                        format_args!(
                            ",\"bytes\":{},\"message\":\"{}\"",
                            len,
                            json_escape(&err.to_string())
                        ),
                    );
                    continue;
                }
            };
            stats.statuses_received = stats.statuses_received.saturating_add(1);
            stats.status_frames_complete = status.frames_complete;
            stats.status_frames_skipped = status.frames_skipped;
            stats.status_last_observed_frame_id = status.last_observed_frame_id;
            if verbose {
                logger.event(
                    "udp_client",
                    "stream_status",
                    format_args!(
                        ",\"last_observed_frame_id\":{},\"frames_complete\":{},\"frames_skipped\":{},\"open_frames\":{},\"chunks_total\":{},\"announce_packets_total\":{}",
                        status.last_observed_frame_id,
                        status.frames_complete,
                        status.frames_skipped,
                        status.open_frames,
                        status.chunks_total,
                        status.announce_packets_total
                    ),
                );
            }
            continue;
        }
        if header.packet_type != UDP_PACKET_NACK {
            continue;
        }
        if header.session_id != session_id {
            logger.event(
                "udp_client",
                "stale_nack",
                format_args!(
                    ",\"frame_id\":{},\"session_id\":{}",
                    header.frame_id, header.session_id
                ),
            );
            continue;
        }

        stats.nacks_received = stats.nacks_received.saturating_add(1);
        let requested = payload.len() / std::mem::size_of::<u32>();
        stats.nack_chunks_requested = stats.nack_chunks_requested.saturating_add(requested as u64);

        let Some(frame) = sent_cache.get(&header.frame_id) else {
            stats.retransmit_misses = stats.retransmit_misses.saturating_add(requested as u64);
            if verbose {
                logger.event(
                    "udp_client",
                    "nack_cache_miss",
                    format_args!(
                        ",\"frame_id\":{},\"requested_chunks\":{}",
                        header.frame_id, requested
                    ),
                );
            }
            continue;
        };

        let mut cursor = 0;
        while cursor < payload.len() {
            let chunk_index = get_u32(payload, &mut cursor)
                .map_err(|err| format!("failed to decode UDP NACK chunk index: {err}"))?
                as usize;
            let Some(chunk) = frame.chunks.get(chunk_index) else {
                stats.retransmit_misses = stats.retransmit_misses.saturating_add(1);
                continue;
            };
            send_udp_datagram(socket, chunk, "retransmit")?;
            stats.retransmit_packets = stats.retransmit_packets.saturating_add(1);
            let payload_bytes = chunk.len().saturating_sub(UDP_PACKET_HEADER_LEN);
            stats.retransmit_bytes = stats.retransmit_bytes.saturating_add(payload_bytes as u64);
        }

        if verbose {
            logger.event(
                "udp_client",
                "nack_handled",
                format_args!(
                    ",\"frame_id\":{},\"requested_chunks\":{}",
                    header.frame_id, requested
                ),
            );
        }
    }
}

fn validate_client_config(config: &ClientConfig) -> Result<(), String> {
    if config.fps == 0 {
        return Err("--fps must be greater than zero".to_string());
    }
    if config.frame_size == 0 {
        return Err("--frame-size must be greater than zero".to_string());
    }
    if config.chunk_size == 0 {
        return Err("--chunk-size must be greater than zero".to_string());
    }
    if config.chunk_size as usize > MAX_RECORD_LEN - CHUNK_BODY_LEN - 1 {
        return Err("--chunk-size is too large".to_string());
    }
    if config.mode == SendMode::Window && config.window_bytes < u64::from(config.chunk_size) {
        return Err("--window-bytes must be at least --chunk-size in window mode".to_string());
    }
    if config.drop_when_window_full && config.mode != SendMode::Window {
        return Err("--drop-when-window-full requires --mode window".to_string());
    }
    if config.drop_when_window_full && config.window_bytes < u64::from(config.frame_size) {
        return Err("--window-bytes must be at least --frame-size when dropping full frames on a full window".to_string());
    }
    Ok(())
}

fn validate_udp_server_config(config: &UdpServerConfig) -> Result<(), String> {
    if config.frame_timeout_ms == 0 {
        return Err("--frame-timeout-ms must be greater than zero".to_string());
    }
    if config.nack_interval_ms == 0 {
        return Err("--nack-interval-ms must be greater than zero".to_string());
    }
    if config.nack_max_chunks == 0 {
        return Err("--nack-max-chunks must be greater than zero".to_string());
    }
    if config.nack_max_chunks as usize * std::mem::size_of::<u32>()
        > MAX_UDP_PACKET_LEN - UDP_PACKET_HEADER_LEN
    {
        return Err("--nack-max-chunks is too large for one UDP packet".to_string());
    }
    Ok(())
}

fn validate_udp_client_config(config: &UdpClientConfig) -> Result<(), String> {
    if config.fps == 0 {
        return Err("--fps must be greater than zero".to_string());
    }
    if config.frame_size == 0 {
        return Err("--frame-size must be greater than zero".to_string());
    }
    if config.payload_size == 0 {
        return Err("--payload-size must be greater than zero".to_string());
    }
    if UDP_PACKET_HEADER_LEN + config.payload_size as usize > MAX_UDP_PACKET_LEN {
        return Err("--payload-size is too large for one UDP packet".to_string());
    }
    Ok(())
}

fn make_ack(frame_id: u64, frame: &FrameRxState, complete: bool) -> FrameAckMsg {
    FrameAckMsg {
        frame_id,
        chunks_seen: frame.chunks_seen,
        chunk_count: frame.chunk_count,
        frame_size: frame.frame_size,
        bytes_seen: frame.bytes_seen,
        complete_unix_us: unix_us(),
        receive_elapsed_us: frame.started.elapsed().as_micros() as u64,
        complete,
    }
}

fn log_open_frames(logger: &Logger, reason: &str, frames: &BTreeMap<u64, FrameRxState>) {
    for (frame_id, frame) in frames {
        logger.event(
            "server",
            "open_frame",
            format_args!(
                ",\"reason\":\"{}\",\"frame_id\":{},\"chunks_seen\":{},\"chunk_count\":{},\"bytes_seen\":{},\"frame_size\":{},\"elapsed_us\":{}",
                json_escape(reason),
                frame_id,
                frame.chunks_seen,
                frame.chunk_count,
                frame.bytes_seen,
                frame.frame_size,
                frame.started.elapsed().as_micros()
            ),
        );
    }
}

fn open_udp_frame_state(
    logger: &Logger,
    event: &str,
    frames: &mut BTreeMap<u64, FrameRxState>,
    completed_frames: &BTreeMap<u64, Instant>,
    header: &UdpVideoPacketHeader,
    first_chunk_index: Option<u32>,
    quiet_frames: bool,
) -> bool {
    if completed_frames.contains_key(&header.frame_id) || frames.contains_key(&header.frame_id) {
        return false;
    }

    let mut seen = Vec::new();
    seen.resize(header.chunk_count as usize, false);
    frames.insert(
        header.frame_id,
        FrameRxState {
            session_id: header.session_id,
            frame_size: header.frame_size,
            chunk_count: header.chunk_count,
            first_unix_us: header.sent_unix_us,
            started: Instant::now(),
            seen,
            chunks_seen: 0,
            bytes_seen: 0,
            last_nack: None,
            nack_rounds: 0,
        },
    );

    if !quiet_frames {
        match first_chunk_index {
            Some(chunk_index) => logger.event(
                "udp_server",
                event,
                format_args!(
                    ",\"frame_id\":{},\"chunk_count\":{},\"frame_size\":{},\"first_chunk_index\":{}",
                    header.frame_id, header.chunk_count, header.frame_size, chunk_index
                ),
            ),
            None => logger.event(
                "udp_server",
                event,
                format_args!(
                    ",\"frame_id\":{},\"chunk_count\":{},\"frame_size\":{}",
                    header.frame_id, header.chunk_count, header.frame_size
                ),
            ),
        }
    }

    true
}

fn open_udp_frame_gaps(
    logger: &Logger,
    frames: &mut BTreeMap<u64, FrameRxState>,
    completed_frames: &BTreeMap<u64, Instant>,
    header: &UdpVideoPacketHeader,
    last_frame_id: u64,
    quiet_frames: bool,
) -> u64 {
    if header.frame_id <= last_frame_id.saturating_add(1) {
        return 0;
    }

    let gap_start = last_frame_id.saturating_add(1);
    let gap_end = header.frame_id;
    let capped_end = gap_start
        .saturating_add(MAX_SYNTHETIC_GAP_FRAMES)
        .min(gap_end);
    let mut opened = 0_u64;

    for frame_id in gap_start..capped_end {
        let gap_header = UdpVideoPacketHeader {
            frame_id,
            chunk_index: 0,
            ..*header
        };
        if open_udp_frame_state(
            logger,
            "frame_gap_opened",
            frames,
            completed_frames,
            &gap_header,
            None,
            quiet_frames,
        ) {
            opened = opened.saturating_add(1);
        }
    }

    if capped_end < gap_end && !quiet_frames {
        logger.event(
            "udp_server",
            "frame_gap_capped",
            format_args!(
                ",\"gap_start\":{},\"gap_end\":{},\"opened\":{},\"cap\":{}",
                gap_start, gap_end, opened, MAX_SYNTHETIC_GAP_FRAMES
            ),
        );
    }

    opened
}

fn expire_udp_frames(
    logger: &Logger,
    reason: &str,
    frames: &mut BTreeMap<u64, FrameRxState>,
    timeout: Duration,
) -> u64 {
    let mut expired = Vec::new();
    for (frame_id, frame) in frames.iter() {
        if frame.started.elapsed() >= timeout {
            expired.push(*frame_id);
        }
    }

    for frame_id in &expired {
        if let Some(frame) = frames.remove(frame_id) {
            logger.event(
                "udp_server",
                "frame_expired",
                format_args!(
                    ",\"reason\":\"{}\",\"frame_id\":{},\"chunks_seen\":{},\"chunk_count\":{},\"missing_chunks\":{},\"bytes_seen\":{},\"frame_size\":{},\"elapsed_us\":{}",
                    json_escape(reason),
                    frame_id,
                    frame.chunks_seen,
                    frame.chunk_count,
                    frame.chunk_count.saturating_sub(frame.chunks_seen),
                    frame.bytes_seen,
                    frame.frame_size,
                    frame.started.elapsed().as_micros()
                ),
            );
        }
    }

    expired.len() as u64
}

fn prune_udp_completed_frames(frames: &mut BTreeMap<u64, Instant>, retention: Duration) {
    let mut stale = Vec::new();
    for (frame_id, completed_at) in frames.iter() {
        if completed_at.elapsed() >= retention {
            stale.push(*frame_id);
        }
    }
    for frame_id in stale {
        frames.remove(&frame_id);
    }
}

fn send_udp_nacks(
    socket: &UdpSocket,
    peer: SocketAddr,
    logger: &Logger,
    frames: &mut BTreeMap<u64, FrameRxState>,
    packet: &mut [u8],
    config: &UdpServerConfig,
) -> (u64, u64) {
    if config.nack_rounds == 0 {
        return (0, 0);
    }

    let mut packets_sent = 0_u64;
    let mut chunks_requested = 0_u64;
    let delay = Duration::from_millis(config.nack_delay_ms);
    let interval = Duration::from_millis(config.nack_interval_ms);
    let now = Instant::now();

    for (frame_id, frame) in frames.iter_mut() {
        if frame.chunks_seen == frame.chunk_count {
            continue;
        }
        if frame.chunks_seen == 0 && !config.nack_empty_frames {
            continue;
        }
        if frame.nack_rounds >= config.nack_rounds {
            continue;
        }
        if frame.started.elapsed() < delay {
            continue;
        }
        if frame
            .last_nack
            .is_some_and(|last_nack| now.duration_since(last_nack) < interval)
        {
            continue;
        }

        let mut cursor = UDP_PACKET_HEADER_LEN;
        let mut missing = 0_u32;
        let max_missing = config.nack_max_chunks.min(frame.chunk_count);
        for (chunk_index, seen) in frame.seen.iter().enumerate() {
            if *seen {
                continue;
            }
            put_u32(packet, &mut cursor, chunk_index as u32);
            missing += 1;
            if missing >= max_missing {
                break;
            }
        }
        if missing == 0 {
            continue;
        }

        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_NACK,
            session_id: frame.session_id,
            frame_id: *frame_id,
            chunk_index: 0,
            chunk_count: frame.chunk_count,
            frame_size: frame.frame_size,
            payload_len: missing * std::mem::size_of::<u32>() as u32,
            sent_unix_us: unix_us(),
        };
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let packet_len = UDP_PACKET_HEADER_LEN + header.payload_len as usize;
        match socket.send_to(&packet[..packet_len], peer) {
            Ok(_) => {
                frame.last_nack = Some(now);
                frame.nack_rounds += 1;
                packets_sent = packets_sent.saturating_add(1);
                chunks_requested = chunks_requested.saturating_add(u64::from(missing));
                if config.verbose_chunks {
                    logger.event(
                        "udp_server",
                        "nack_sent",
                        format_args!(
                            ",\"frame_id\":{},\"missing_chunks\":{},\"round\":{}",
                            frame_id, missing, frame.nack_rounds
                        ),
                    );
                }
            }
            Err(err) => {
                logger.event(
                    "udp_server",
                    "nack_send_error",
                    format_args!(
                        ",\"frame_id\":{},\"missing_chunks\":{},\"message\":\"{}\"",
                        frame_id,
                        missing,
                        json_escape(&err.to_string())
                    ),
                );
            }
        }
    }

    (packets_sent, chunks_requested)
}

fn encode_udp_packet_header(header: &UdpVideoPacketHeader, out: &mut [u8]) {
    debug_assert!(out.len() >= UDP_PACKET_HEADER_LEN);
    let mut cursor = 0;
    put_u32(out, &mut cursor, MAGIC);
    put_u16(out, &mut cursor, VERSION);
    out[cursor] = header.packet_type;
    cursor += 1;
    put_u64(out, &mut cursor, header.session_id);
    put_u64(out, &mut cursor, header.frame_id);
    put_u32(out, &mut cursor, header.chunk_index);
    put_u32(out, &mut cursor, header.chunk_count);
    put_u32(out, &mut cursor, header.frame_size);
    put_u32(out, &mut cursor, header.payload_len);
    put_u64(out, &mut cursor, header.sent_unix_us);
    debug_assert_eq!(cursor, UDP_PACKET_HEADER_LEN);
}

fn encode_udp_status(status: &UdpStatusMsg, out: &mut [u8]) {
    debug_assert!(out.len() >= UDP_STATUS_BODY_LEN);
    let mut cursor = 0;
    put_u64(out, &mut cursor, status.last_observed_frame_id);
    put_u64(out, &mut cursor, status.frames_complete);
    put_u64(out, &mut cursor, status.frames_skipped);
    put_u32(out, &mut cursor, status.open_frames);
    put_u64(out, &mut cursor, status.chunks_total);
    put_u64(out, &mut cursor, status.announce_packets_total);
    debug_assert_eq!(cursor, UDP_STATUS_BODY_LEN);
}

fn decode_udp_status(data: &[u8]) -> io::Result<UdpStatusMsg> {
    if data.len() != UDP_STATUS_BODY_LEN {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP status payload has invalid length",
        ));
    }
    let mut cursor = 0;
    Ok(UdpStatusMsg {
        last_observed_frame_id: get_u64(data, &mut cursor)?,
        frames_complete: get_u64(data, &mut cursor)?,
        frames_skipped: get_u64(data, &mut cursor)?,
        open_frames: get_u32(data, &mut cursor)?,
        chunks_total: get_u64(data, &mut cursor)?,
        announce_packets_total: get_u64(data, &mut cursor)?,
    })
}

fn decode_udp_packet(data: &[u8]) -> io::Result<(UdpVideoPacketHeader, &[u8])> {
    if data.len() < UDP_PACKET_HEADER_LEN {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP packet is shorter than header",
        ));
    }
    let mut cursor = 0;
    let magic = get_u32(data, &mut cursor)?;
    let version = get_u16(data, &mut cursor)?;
    if magic != MAGIC || version != VERSION {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "unsupported UDP protocol version",
        ));
    }
    let packet_type = data[cursor];
    cursor += 1;
    if packet_type != UDP_PACKET_VIDEO
        && packet_type != UDP_PACKET_BYE
        && packet_type != UDP_PACKET_NACK
        && packet_type != UDP_PACKET_ANNOUNCE
        && packet_type != UDP_PACKET_STATUS
    {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "unknown UDP packet type",
        ));
    }
    let header = UdpVideoPacketHeader {
        packet_type,
        session_id: get_u64(data, &mut cursor)?,
        frame_id: get_u64(data, &mut cursor)?,
        chunk_index: get_u32(data, &mut cursor)?,
        chunk_count: get_u32(data, &mut cursor)?,
        frame_size: get_u32(data, &mut cursor)?,
        payload_len: get_u32(data, &mut cursor)?,
        sent_unix_us: get_u64(data, &mut cursor)?,
    };
    debug_assert_eq!(cursor, UDP_PACKET_HEADER_LEN);
    let payload = &data[UDP_PACKET_HEADER_LEN..];
    if payload.len() != header.payload_len as usize {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP payload length mismatch",
        ));
    }
    if header.packet_type == UDP_PACKET_BYE && header.payload_len != 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP bye packet must not have payload",
        ));
    }
    if header.packet_type == UDP_PACKET_ANNOUNCE && header.payload_len != 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP announce packet must not have payload",
        ));
    }
    if header.packet_type == UDP_PACKET_NACK
        && header.payload_len as usize % std::mem::size_of::<u32>() != 0
    {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP nack payload must contain u32 chunk indexes",
        ));
    }
    if header.packet_type == UDP_PACKET_STATUS && header.payload_len as usize != UDP_STATUS_BODY_LEN
    {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "UDP status packet has invalid payload length",
        ));
    }
    Ok((header, payload))
}

fn write_hello(stream: &mut TcpStream, hello: &HelloMsg) -> io::Result<()> {
    let mut body = [0_u8; HELLO_BODY_LEN];
    let mut cursor = 0;
    put_u32(&mut body, &mut cursor, MAGIC);
    put_u16(&mut body, &mut cursor, VERSION);
    put_u32(&mut body, &mut cursor, hello.chunk_size);
    put_u32(&mut body, &mut cursor, hello.frame_size);
    put_u32(&mut body, &mut cursor, hello.fps);
    put_u32(&mut body, &mut cursor, hello.duration_sec);
    put_u32(&mut body, &mut cursor, hello.window_bytes);
    body[cursor] = hello.mode;
    write_record(stream, MSG_HELLO, &body, &[])
}

fn write_chunk(stream: &mut TcpStream, chunk: &ChunkMsg, payload: &[u8]) -> io::Result<()> {
    let mut body = [0_u8; CHUNK_BODY_LEN];
    let mut cursor = 0;
    put_u64(&mut body, &mut cursor, chunk.frame_id);
    put_u32(&mut body, &mut cursor, chunk.chunk_index);
    put_u32(&mut body, &mut cursor, chunk.chunk_count);
    put_u32(&mut body, &mut cursor, chunk.frame_size);
    put_u64(&mut body, &mut cursor, chunk.sent_unix_us);
    put_u32(&mut body, &mut cursor, chunk.payload_len);
    write_record(stream, MSG_CHUNK, &body, payload)
}

fn write_ack(stream: &mut TcpStream, ack: &FrameAckMsg) -> io::Result<()> {
    let mut body = [0_u8; ACK_BODY_LEN];
    let mut cursor = 0;
    put_u64(&mut body, &mut cursor, ack.frame_id);
    put_u32(&mut body, &mut cursor, ack.chunks_seen);
    put_u32(&mut body, &mut cursor, ack.chunk_count);
    put_u32(&mut body, &mut cursor, ack.frame_size);
    put_u32(&mut body, &mut cursor, ack.bytes_seen);
    put_u64(&mut body, &mut cursor, ack.complete_unix_us);
    put_u64(&mut body, &mut cursor, ack.receive_elapsed_us);
    body[cursor] = u8::from(ack.complete);
    write_record(stream, MSG_FRAME_ACK, &body, &[])
}

fn write_bye(stream: &mut TcpStream) -> io::Result<()> {
    write_record(stream, MSG_BYE, &[], &[])
}

fn write_record(stream: &mut TcpStream, kind: u8, body: &[u8], payload: &[u8]) -> io::Result<()> {
    let len = 1_usize
        .checked_add(body.len())
        .and_then(|value| value.checked_add(payload.len()))
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "record length overflow"))?;
    if len > MAX_RECORD_LEN {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "record length exceeds maximum",
        ));
    }
    stream.write_all(&(len as u32).to_be_bytes())?;
    stream.write_all(&[kind])?;
    stream.write_all(body)?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_wire_message<R: Read>(
    reader: &mut R,
    scratch: &mut Vec<u8>,
) -> io::Result<Option<WireMessage>> {
    let mut len_bytes = [0_u8; 4];
    match reader.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len == 0 || len > MAX_RECORD_LEN {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid record length",
        ));
    }
    scratch.resize(len, 0);
    reader.read_exact(scratch)?;
    let kind = scratch[0];
    let data = &scratch[1..];
    match kind {
        MSG_HELLO => {
            if data.len() != HELLO_BODY_LEN {
                return Err(io::Error::new(ErrorKind::InvalidData, "bad hello length"));
            }
            let mut cursor = 0;
            let magic = get_u32(data, &mut cursor)?;
            let version = get_u16(data, &mut cursor)?;
            if magic != MAGIC || version != VERSION {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "unsupported protocol version",
                ));
            }
            Ok(Some(WireMessage::Hello(HelloMsg {
                chunk_size: get_u32(data, &mut cursor)?,
                frame_size: get_u32(data, &mut cursor)?,
                fps: get_u32(data, &mut cursor)?,
                duration_sec: get_u32(data, &mut cursor)?,
                window_bytes: get_u32(data, &mut cursor)?,
                mode: data[cursor],
            })))
        }
        MSG_CHUNK => {
            if data.len() < CHUNK_BODY_LEN {
                return Err(io::Error::new(ErrorKind::InvalidData, "bad chunk length"));
            }
            let mut cursor = 0;
            let chunk = ChunkMsg {
                frame_id: get_u64(data, &mut cursor)?,
                chunk_index: get_u32(data, &mut cursor)?,
                chunk_count: get_u32(data, &mut cursor)?,
                frame_size: get_u32(data, &mut cursor)?,
                sent_unix_us: get_u64(data, &mut cursor)?,
                payload_len: get_u32(data, &mut cursor)?,
            };
            let payload_len = data.len() - CHUNK_BODY_LEN;
            if payload_len != chunk.payload_len as usize {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "chunk payload length mismatch",
                ));
            }
            Ok(Some(WireMessage::Chunk(chunk)))
        }
        MSG_FRAME_ACK => {
            if data.len() != ACK_BODY_LEN {
                return Err(io::Error::new(ErrorKind::InvalidData, "bad ack length"));
            }
            let mut cursor = 0;
            Ok(Some(WireMessage::FrameAck(FrameAckMsg {
                frame_id: get_u64(data, &mut cursor)?,
                chunks_seen: get_u32(data, &mut cursor)?,
                chunk_count: get_u32(data, &mut cursor)?,
                frame_size: get_u32(data, &mut cursor)?,
                bytes_seen: get_u32(data, &mut cursor)?,
                complete_unix_us: get_u64(data, &mut cursor)?,
                receive_elapsed_us: get_u64(data, &mut cursor)?,
                complete: data[cursor] != 0,
            })))
        }
        MSG_BYE => {
            if !data.is_empty() {
                return Err(io::Error::new(ErrorKind::InvalidData, "bad bye length"));
            }
            Ok(Some(WireMessage::Bye))
        }
        _ => Err(io::Error::new(
            ErrorKind::InvalidData,
            "unknown message kind",
        )),
    }
}

fn put_u16(out: &mut [u8], cursor: &mut usize, value: u16) {
    out[*cursor..*cursor + 2].copy_from_slice(&value.to_be_bytes());
    *cursor += 2;
}

fn put_u32(out: &mut [u8], cursor: &mut usize, value: u32) {
    out[*cursor..*cursor + 4].copy_from_slice(&value.to_be_bytes());
    *cursor += 4;
}

fn put_u64(out: &mut [u8], cursor: &mut usize, value: u64) {
    out[*cursor..*cursor + 8].copy_from_slice(&value.to_be_bytes());
    *cursor += 8;
}

fn get_u16(data: &[u8], cursor: &mut usize) -> io::Result<u16> {
    if data.len().saturating_sub(*cursor) < 2 {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, "u16 truncated"));
    }
    let value = u16::from_be_bytes([data[*cursor], data[*cursor + 1]]);
    *cursor += 2;
    Ok(value)
}

fn get_u32(data: &[u8], cursor: &mut usize) -> io::Result<u32> {
    if data.len().saturating_sub(*cursor) < 4 {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, "u32 truncated"));
    }
    let value = u32::from_be_bytes([
        data[*cursor],
        data[*cursor + 1],
        data[*cursor + 2],
        data[*cursor + 3],
    ]);
    *cursor += 4;
    Ok(value)
}

fn get_u64(data: &[u8], cursor: &mut usize) -> io::Result<u64> {
    if data.len().saturating_sub(*cursor) < 8 {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, "u64 truncated"));
    }
    let value = u64::from_be_bytes([
        data[*cursor],
        data[*cursor + 1],
        data[*cursor + 2],
        data[*cursor + 3],
        data[*cursor + 4],
        data[*cursor + 5],
        data[*cursor + 6],
        data[*cursor + 7],
    ]);
    *cursor += 8;
    Ok(value)
}

fn fill_payload(payload: &mut [u8], frame_id: u64, chunk_index: u32) {
    let seed = frame_id.wrapping_mul(1_315_423_911) ^ u64::from(chunk_index);
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u64).to_le_bytes()[0];
    }
}

fn div_ceil_u32(value: u32, divisor: u32) -> u32 {
    (value / divisor) + u32::from(value % divisor != 0)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn parse_args() -> Result<Command, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        return Err(usage());
    }
    match args[0].as_str() {
        "server" => parse_server(&args[1..]).map(Command::Server),
        "client" => parse_client(&args[1..]).map(Command::Client),
        "udp-server" => parse_udp_server(&args[1..]).map(Command::UdpServer),
        "udp-client" => parse_udp_client(&args[1..]).map(Command::UdpClient),
        other => Err(format!("unknown command '{other}'\n\n{}", usage())),
    }
}

fn parse_server(args: &[String]) -> Result<ServerConfig, String> {
    let mut config = ServerConfig {
        bind: "0.0.0.0:23000".to_string(),
        log_path: None,
        ack_every_chunks: 0,
        read_timeout_ms: 30_000,
        pause_read_after_ms: 0,
        pause_read_duration_ms: 0,
        pause_read_repeat_ms: 0,
        verbose_chunks: false,
        tcp_nodelay: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => config.bind = next_value(args, &mut i, "--bind")?,
            "--log" => config.log_path = Some(next_value(args, &mut i, "--log")?),
            "--ack-every-chunks" => {
                config.ack_every_chunks =
                    parse_u32(&next_value(args, &mut i, "--ack-every-chunks")?)?
            }
            "--read-timeout-ms" => {
                config.read_timeout_ms = parse_u64(&next_value(args, &mut i, "--read-timeout-ms")?)?
            }
            "--pause-read-after-ms" => {
                config.pause_read_after_ms =
                    parse_u64(&next_value(args, &mut i, "--pause-read-after-ms")?)?
            }
            "--pause-read-duration-ms" => {
                config.pause_read_duration_ms =
                    parse_u64(&next_value(args, &mut i, "--pause-read-duration-ms")?)?
            }
            "--pause-read-repeat-ms" => {
                config.pause_read_repeat_ms =
                    parse_u64(&next_value(args, &mut i, "--pause-read-repeat-ms")?)?
            }
            "--verbose-chunks" => config.verbose_chunks = true,
            "--tcp-nodelay" => {
                config.tcp_nodelay = parse_bool(&next_value(args, &mut i, "--tcp-nodelay")?)?
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown server option '{other}'")),
        }
        i += 1;
    }
    Ok(config)
}

fn parse_client(args: &[String]) -> Result<ClientConfig, String> {
    let mut connect = None;
    let mut config = ClientConfig {
        connect: String::new(),
        log_path: None,
        duration_sec: 20,
        fps: 30,
        frame_size: 43 * 1024,
        chunk_size: 1024,
        mode: SendMode::Burst,
        window_bytes: 256 * 1024,
        window_wait_ms: 10_000,
        drop_when_window_full: false,
        pace_every: 4,
        pace_us: 1000,
        io_timeout_ms: 15_000,
        verbose_chunks: false,
        tcp_nodelay: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--connect" => connect = Some(next_value(args, &mut i, "--connect")?),
            "--log" => config.log_path = Some(next_value(args, &mut i, "--log")?),
            "--duration-sec" => {
                config.duration_sec = parse_u64(&next_value(args, &mut i, "--duration-sec")?)?
            }
            "--fps" => config.fps = parse_u32(&next_value(args, &mut i, "--fps")?)?,
            "--frame-size" => {
                config.frame_size = parse_u32(&next_value(args, &mut i, "--frame-size")?)?
            }
            "--chunk-size" => {
                config.chunk_size = parse_u32(&next_value(args, &mut i, "--chunk-size")?)?
            }
            "--mode" => config.mode = SendMode::parse(&next_value(args, &mut i, "--mode")?)?,
            "--window-bytes" => {
                config.window_bytes = parse_u64(&next_value(args, &mut i, "--window-bytes")?)?
            }
            "--window-wait-ms" => {
                config.window_wait_ms = parse_u64(&next_value(args, &mut i, "--window-wait-ms")?)?
            }
            "--drop-when-window-full" => config.drop_when_window_full = true,
            "--pace-every" => {
                config.pace_every = parse_u32(&next_value(args, &mut i, "--pace-every")?)?
            }
            "--pace-us" => config.pace_us = parse_u64(&next_value(args, &mut i, "--pace-us")?)?,
            "--io-timeout-ms" => {
                config.io_timeout_ms = parse_u64(&next_value(args, &mut i, "--io-timeout-ms")?)?
            }
            "--verbose-chunks" => config.verbose_chunks = true,
            "--tcp-nodelay" => {
                config.tcp_nodelay = parse_bool(&next_value(args, &mut i, "--tcp-nodelay")?)?
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown client option '{other}'")),
        }
        i += 1;
    }
    config.connect = connect.ok_or_else(|| "--connect is required for client mode".to_string())?;
    Ok(config)
}

fn parse_udp_server(args: &[String]) -> Result<UdpServerConfig, String> {
    let mut config = UdpServerConfig {
        bind: "0.0.0.0:23000".to_string(),
        log_path: None,
        read_timeout_ms: 30_000,
        frame_timeout_ms: 2_000,
        drop_every: 0,
        drop_initial_frame_video_every: 0,
        verbose_chunks: false,
        quiet_frames: false,
        nack_delay_ms: 25,
        nack_interval_ms: 25,
        nack_rounds: 8,
        nack_max_chunks: 128,
        nack_empty_frames: false,
        status_interval_ms: 500,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => config.bind = next_value(args, &mut i, "--bind")?,
            "--log" => config.log_path = Some(next_value(args, &mut i, "--log")?),
            "--read-timeout-ms" => {
                config.read_timeout_ms = parse_u64(&next_value(args, &mut i, "--read-timeout-ms")?)?
            }
            "--frame-timeout-ms" => {
                config.frame_timeout_ms =
                    parse_u64(&next_value(args, &mut i, "--frame-timeout-ms")?)?
            }
            "--drop-every" => {
                config.drop_every = parse_u64(&next_value(args, &mut i, "--drop-every")?)?
            }
            "--drop-initial-frame-video-every" => {
                config.drop_initial_frame_video_every = parse_u64(&next_value(
                    args,
                    &mut i,
                    "--drop-initial-frame-video-every",
                )?)?
            }
            "--verbose-chunks" => config.verbose_chunks = true,
            "--quiet-frames" => config.quiet_frames = true,
            "--nack-delay-ms" => {
                config.nack_delay_ms = parse_u64(&next_value(args, &mut i, "--nack-delay-ms")?)?
            }
            "--nack-interval-ms" => {
                config.nack_interval_ms =
                    parse_u64(&next_value(args, &mut i, "--nack-interval-ms")?)?
            }
            "--nack-rounds" => {
                config.nack_rounds = parse_u32(&next_value(args, &mut i, "--nack-rounds")?)?
            }
            "--nack-max-chunks" => {
                config.nack_max_chunks = parse_u32(&next_value(args, &mut i, "--nack-max-chunks")?)?
            }
            "--nack-empty-frames" => config.nack_empty_frames = true,
            "--status-interval-ms" => {
                config.status_interval_ms =
                    parse_u64(&next_value(args, &mut i, "--status-interval-ms")?)?
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown udp-server option '{other}'")),
        }
        i += 1;
    }
    Ok(config)
}

fn parse_udp_client(args: &[String]) -> Result<UdpClientConfig, String> {
    let mut connect = None;
    let mut config = UdpClientConfig {
        connect: String::new(),
        bind: "0.0.0.0:0".to_string(),
        log_path: None,
        duration_sec: 20,
        fps: 30,
        frame_size: 43 * 1024,
        payload_size: 1100,
        pace_every: 0,
        pace_us: 0,
        verbose_chunks: false,
        quiet_frames: false,
        resend_cache_frames: 180,
        nack_linger_ms: 1_000,
        announce_frames: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--connect" => connect = Some(next_value(args, &mut i, "--connect")?),
            "--bind" => config.bind = next_value(args, &mut i, "--bind")?,
            "--log" => config.log_path = Some(next_value(args, &mut i, "--log")?),
            "--duration-sec" => {
                config.duration_sec = parse_u64(&next_value(args, &mut i, "--duration-sec")?)?
            }
            "--fps" => config.fps = parse_u32(&next_value(args, &mut i, "--fps")?)?,
            "--frame-size" => {
                config.frame_size = parse_u32(&next_value(args, &mut i, "--frame-size")?)?
            }
            "--payload-size" => {
                config.payload_size = parse_u32(&next_value(args, &mut i, "--payload-size")?)?
            }
            "--pace-every" => {
                config.pace_every = parse_u32(&next_value(args, &mut i, "--pace-every")?)?
            }
            "--pace-us" => config.pace_us = parse_u64(&next_value(args, &mut i, "--pace-us")?)?,
            "--verbose-chunks" => config.verbose_chunks = true,
            "--quiet-frames" => config.quiet_frames = true,
            "--resend-cache-frames" => {
                config.resend_cache_frames =
                    parse_usize(&next_value(args, &mut i, "--resend-cache-frames")?)?
            }
            "--nack-linger-ms" => {
                config.nack_linger_ms = parse_u64(&next_value(args, &mut i, "--nack-linger-ms")?)?
            }
            "--no-announce" => config.announce_frames = false,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown udp-client option '{other}'")),
        }
        i += 1;
    }
    config.connect =
        connect.ok_or_else(|| "--connect is required for udp-client mode".to_string())?;
    Ok(config)
}

fn next_value(args: &[String], index: &mut usize, name: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_u32(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|err| format!("invalid integer '{value}': {err}"))
}

fn parse_u64(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|err| format!("invalid integer '{value}': {err}"))
}

fn parse_usize(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|err| format!("invalid integer '{value}': {err}"))
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("invalid boolean '{value}'")),
    }
}

fn usage() -> String {
    "usage:
  rustadmin-netprobe server [--bind 0.0.0.0:23000] [--log PATH] [--ack-every-chunks N] [--read-timeout-ms N] [--pause-read-after-ms N] [--pause-read-duration-ms N] [--pause-read-repeat-ms N] [--verbose-chunks] [--tcp-nodelay true|false]
  rustadmin-netprobe client --connect HOST:23000 [--duration-sec N] [--fps N] [--frame-size BYTES] [--chunk-size BYTES] [--mode burst|paced|window] [--window-bytes BYTES] [--window-wait-ms N] [--drop-when-window-full] [--pace-every N] [--pace-us N] [--io-timeout-ms N] [--log PATH] [--verbose-chunks] [--tcp-nodelay true|false]
  rustadmin-netprobe udp-server [--bind 0.0.0.0:23000] [--log PATH] [--read-timeout-ms N] [--frame-timeout-ms N] [--drop-every N] [--drop-initial-frame-video-every N] [--nack-delay-ms N] [--nack-interval-ms N] [--nack-rounds N] [--nack-max-chunks N] [--nack-empty-frames] [--status-interval-ms N] [--quiet-frames] [--verbose-chunks]
  rustadmin-netprobe udp-client --connect HOST:23000 [--bind 0.0.0.0:0] [--duration-sec N] [--fps N] [--frame-size BYTES] [--payload-size BYTES] [--pace-every N] [--pace-us N] [--resend-cache-frames N] [--nack-linger-ms N] [--no-announce] [--log PATH] [--quiet-frames] [--verbose-chunks]
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn chunk_count_rounds_up() {
        assert_eq!(div_ceil_u32(43_090, 1024), 43);
        assert_eq!(div_ceil_u32(1024, 1024), 1);
        assert_eq!(div_ceil_u32(1025, 1024), 2);
    }

    #[test]
    fn hello_round_trip() {
        let hello = HelloMsg {
            chunk_size: 1024,
            frame_size: 43_090,
            fps: 30,
            duration_sec: 20,
            window_bytes: 262_144,
            mode: SendMode::Window.wire(),
        };
        let mut bytes = Vec::new();
        write_hello_to_vec(&mut bytes, &hello).unwrap();
        let mut cursor = Cursor::new(bytes);
        let mut scratch = Vec::new();
        let message = read_wire_message(&mut cursor, &mut scratch)
            .unwrap()
            .unwrap();
        match message {
            WireMessage::Hello(decoded) => {
                assert_eq!(decoded.chunk_size, hello.chunk_size);
                assert_eq!(decoded.frame_size, hello.frame_size);
                assert_eq!(decoded.mode, hello.mode);
            }
            _ => panic!("unexpected message"),
        }
    }

    #[test]
    fn chunk_rejects_payload_mismatch() {
        let mut body = [0_u8; CHUNK_BODY_LEN];
        let mut cursor = 0;
        put_u64(&mut body, &mut cursor, 1);
        put_u32(&mut body, &mut cursor, 0);
        put_u32(&mut body, &mut cursor, 2);
        put_u32(&mut body, &mut cursor, 2048);
        put_u64(&mut body, &mut cursor, 123);
        put_u32(&mut body, &mut cursor, 1024);

        let mut bytes = Vec::new();
        write_record_to_vec(&mut bytes, MSG_CHUNK, &body, &[0_u8; 16]).unwrap();
        let mut cursor = Cursor::new(bytes);
        let mut scratch = Vec::new();
        let err = read_wire_message(&mut cursor, &mut scratch).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn parses_tcp_pause_options() {
        let args = strings(&[
            "--pause-read-after-ms",
            "3000",
            "--pause-read-duration-ms",
            "12000",
            "--pause-read-repeat-ms",
            "60000",
        ]);
        let config = parse_server(&args).unwrap();
        assert_eq!(config.pause_read_after_ms, 3000);
        assert_eq!(config.pause_read_duration_ms, 12000);
        assert_eq!(config.pause_read_repeat_ms, 60000);
    }

    #[test]
    fn parses_tcp_window_drop_options() {
        let args = strings(&[
            "--connect",
            "127.0.0.1:23000",
            "--mode",
            "window",
            "--window-bytes",
            "262144",
            "--window-wait-ms",
            "0",
            "--drop-when-window-full",
        ]);
        let config = parse_client(&args).unwrap();
        assert_eq!(config.mode, SendMode::Window);
        assert_eq!(config.window_bytes, 262_144);
        assert_eq!(config.window_wait_ms, 0);
        assert!(config.drop_when_window_full);
        validate_client_config(&config).unwrap();
    }

    #[test]
    fn window_drop_requires_full_frame_capacity() {
        let args = strings(&[
            "--connect",
            "127.0.0.1:23000",
            "--mode",
            "window",
            "--frame-size",
            "43090",
            "--window-bytes",
            "4096",
            "--drop-when-window-full",
        ]);
        let config = parse_client(&args).unwrap();
        assert!(validate_client_config(&config).is_err());
    }

    #[test]
    fn udp_packet_round_trip() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_VIDEO,
            session_id: 7,
            frame_id: 11,
            chunk_index: 3,
            chunk_count: 9,
            frame_size: 4096,
            payload_len: 4,
            sent_unix_us: 1234,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 4];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        packet[UDP_PACKET_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

        let (decoded, payload) = decode_udp_packet(&packet).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn udp_packet_rejects_payload_mismatch() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_VIDEO,
            session_id: 7,
            frame_id: 11,
            chunk_index: 3,
            chunk_count: 9,
            frame_size: 4096,
            payload_len: 8,
            sent_unix_us: 1234,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 4];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let err = decode_udp_packet(&packet).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn udp_nack_packet_round_trip() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_NACK,
            session_id: 99,
            frame_id: 42,
            chunk_index: 0,
            chunk_count: 40,
            frame_size: 43_090,
            payload_len: 8,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 8];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let mut cursor = UDP_PACKET_HEADER_LEN;
        put_u32(&mut packet, &mut cursor, 7);
        put_u32(&mut packet, &mut cursor, 13);

        let (decoded, payload) = decode_udp_packet(&packet).unwrap();
        assert_eq!(decoded, header);
        let mut cursor = 0;
        assert_eq!(get_u32(payload, &mut cursor).unwrap(), 7);
        assert_eq!(get_u32(payload, &mut cursor).unwrap(), 13);
    }

    #[test]
    fn udp_announce_packet_round_trip() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_ANNOUNCE,
            session_id: 99,
            frame_id: 42,
            chunk_index: 0,
            chunk_count: 40,
            frame_size: 43_090,
            payload_len: 0,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN];
        encode_udp_packet_header(&header, &mut packet);

        let (decoded, payload) = decode_udp_packet(&packet).unwrap();
        assert_eq!(decoded, header);
        assert!(payload.is_empty());
    }

    #[test]
    fn udp_announce_rejects_payload() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_ANNOUNCE,
            session_id: 99,
            frame_id: 42,
            chunk_index: 0,
            chunk_count: 40,
            frame_size: 43_090,
            payload_len: 4,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 4];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let err = decode_udp_packet(&packet).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn udp_status_packet_round_trip() {
        let status = UdpStatusMsg {
            last_observed_frame_id: 99,
            frames_complete: 88,
            frames_skipped: 7,
            open_frames: 4,
            chunks_total: 1234,
            announce_packets_total: 96,
        };
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_STATUS,
            session_id: 55,
            frame_id: status.last_observed_frame_id,
            chunk_index: 0,
            chunk_count: 0,
            frame_size: 0,
            payload_len: UDP_STATUS_BODY_LEN as u32,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + UDP_STATUS_BODY_LEN];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        encode_udp_status(&status, &mut packet[UDP_PACKET_HEADER_LEN..]);

        let (decoded, payload) = decode_udp_packet(&packet).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decode_udp_status(payload).unwrap(), status);
    }

    #[test]
    fn udp_status_rejects_bad_payload_len() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_STATUS,
            session_id: 55,
            frame_id: 99,
            chunk_index: 0,
            chunk_count: 0,
            frame_size: 0,
            payload_len: 4,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 4];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let err = decode_udp_packet(&packet).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn udp_nack_rejects_partial_chunk_index() {
        let header = UdpVideoPacketHeader {
            packet_type: UDP_PACKET_NACK,
            session_id: 99,
            frame_id: 42,
            chunk_index: 0,
            chunk_count: 40,
            frame_size: 43_090,
            payload_len: 3,
            sent_unix_us: 5678,
        };
        let mut packet = vec![0_u8; UDP_PACKET_HEADER_LEN + 3];
        encode_udp_packet_header(&header, &mut packet[..UDP_PACKET_HEADER_LEN]);
        let err = decode_udp_packet(&packet).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    fn write_hello_to_vec(out: &mut Vec<u8>, hello: &HelloMsg) -> io::Result<()> {
        let mut body = [0_u8; HELLO_BODY_LEN];
        let mut cursor = 0;
        put_u32(&mut body, &mut cursor, MAGIC);
        put_u16(&mut body, &mut cursor, VERSION);
        put_u32(&mut body, &mut cursor, hello.chunk_size);
        put_u32(&mut body, &mut cursor, hello.frame_size);
        put_u32(&mut body, &mut cursor, hello.fps);
        put_u32(&mut body, &mut cursor, hello.duration_sec);
        put_u32(&mut body, &mut cursor, hello.window_bytes);
        body[cursor] = hello.mode;
        write_record_to_vec(out, MSG_HELLO, &body, &[])
    }

    fn write_record_to_vec(
        out: &mut Vec<u8>,
        kind: u8,
        body: &[u8],
        payload: &[u8],
    ) -> io::Result<()> {
        let len = 1 + body.len() + payload.len();
        out.write_all(&(len as u32).to_be_bytes())?;
        out.write_all(&[kind])?;
        out.write_all(body)?;
        out.write_all(payload)?;
        Ok(())
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }
}
