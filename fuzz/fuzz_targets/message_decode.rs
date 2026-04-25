#![no_main]

use hbb_common::{message_proto::Message, protobuf::Message as _};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = Message::parse_from_bytes(data);
});
