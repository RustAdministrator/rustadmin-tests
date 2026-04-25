#![no_main]

use hbb_common::compress::decompress;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() <= 1024 * 1024 {
        let _ = decompress(data);
    }
});
