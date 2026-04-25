#![no_main]

use hbb_common::{protobuf::Message as _, rendezvous_proto::RendezvousMessage};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = RendezvousMessage::parse_from_bytes(data);
});
