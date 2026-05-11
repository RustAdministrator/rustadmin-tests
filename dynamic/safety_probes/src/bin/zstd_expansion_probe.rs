use hbb_common::compress::{compress, decompress, DEFAULT_DECOMPRESS_MAX_LEN};
use std::{process::ExitCode, time::Instant};

fn main() -> ExitCode {
    let plain_len = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8 * 1024 * 1024);
    let input = vec![0u8; plain_len];

    let started = Instant::now();
    let compressed = compress(&input);
    let compress_elapsed_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let output = decompress(&compressed);
    let decompress_elapsed_ms = started.elapsed().as_millis();

    let ratio = if compressed.is_empty() {
        0.0
    } else {
        output.len() as f64 / compressed.len() as f64
    };

    println!("probe=zstd_expansion");
    println!("plain_len={plain_len}");
    println!("default_decompress_max_len={DEFAULT_DECOMPRESS_MAX_LEN}");
    println!("compressed_len={}", compressed.len());
    println!("decompressed_len={}", output.len());
    println!("expansion_ratio={ratio:.2}");
    println!("compress_elapsed_ms={compress_elapsed_ms}");
    println!("decompress_elapsed_ms={decompress_elapsed_ms}");
    println!(
        "finding_reproduced={}",
        output.len() == plain_len && ratio > 100.0
    );

    ExitCode::SUCCESS
}
