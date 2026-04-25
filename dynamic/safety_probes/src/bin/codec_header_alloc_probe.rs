use hbb_common::{bytes::BytesMut, bytes_codec::BytesCodec, tokio_util::codec::Decoder};
use std::process::ExitCode;

fn encode_head_only(len: usize) -> [u8; 4] {
    let encoded = ((len as u32) << 2) | 0x3;
    encoded.to_le_bytes()
}

fn main() -> ExitCode {
    let advertised_len = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64 * 1024 * 1024);

    let header = encode_head_only(advertised_len);
    let mut buf = BytesMut::from(&header[..]);
    let before_capacity = buf.capacity();
    let mut codec = BytesCodec::new();
    let result = codec.decode(&mut buf);
    let after_capacity = buf.capacity();
    let capacity_growth = after_capacity.saturating_sub(before_capacity);
    let reproduced = capacity_growth > 1024 * 1024;

    println!("probe=codec_header_alloc");
    println!("advertised_len={advertised_len}");
    println!(
        "decode_result={:?}",
        result.map(|value| value.map(|frame| frame.len()))
    );
    println!("before_capacity={before_capacity}");
    println!("after_capacity={after_capacity}");
    println!("capacity_growth={capacity_growth}");
    println!("finding_reproduced={reproduced}");

    if reproduced && std::env::var("EXPECT_HARDENED").ok().as_deref() == Some("1") {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
