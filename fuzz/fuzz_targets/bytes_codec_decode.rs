#![no_main]

use hbb_common::{bytes::BytesMut, bytes_codec::BytesCodec, tokio_util::codec::Decoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut codec = BytesCodec::new();
    codec.set_max_packet_length(1024 * 1024);

    let mut buf = BytesMut::from(data);
    for _ in 0..64 {
        match codec.decode(&mut buf) {
            Ok(Some(_frame)) => {}
            Ok(None) | Err(_) => break,
        }
        if buf.is_empty() {
            break;
        }
    }
});
