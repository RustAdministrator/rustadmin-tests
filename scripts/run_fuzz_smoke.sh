#!/usr/bin/env bash
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SECONDS_PER_TARGET="${1:-60}"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$ROOT/rustdesk-tests/results/$STAMP/fuzz"
SUMMARY="$ROOT/rustdesk-tests/results/$STAMP/summary.md"
FUZZ_DIR="$ROOT/rustdesk-tests"
mkdir -p "$OUT"

{
    echo "# Safety Test Summary - $STAMP"
    echo
    echo "## Fuzz Smoke"
} >"$SUMMARY"

if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "- cargo-fuzz: missing" >>"$SUMMARY"
    echo "cargo-fuzz is missing; install with: cargo install cargo-fuzz" | tee "$OUT/missing_tool.log"
    exit 0
fi

if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    cargo_fuzz=(cargo +nightly fuzz)
else
    cargo_fuzz=(cargo fuzz)
    echo "- nightly toolchain: not found; cargo-fuzz may fail because sanitizer fuzzing requires nightly" >>"$SUMMARY"
fi

targets=(
    bytes_codec_decode
    rendezvous_message_decode
    message_decode
    file_transfer_paths
    zstd_decompress
)

for target in "${targets[@]}"; do
    log="$OUT/${target}.log"
    (
        cd "$FUZZ_DIR" || exit 1
        ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=0:detect_odr_violation=0}" \
            "${cargo_fuzz[@]}" run "$target" -- -max_total_time="$SECONDS_PER_TARGET"
    ) >"$log" 2>&1
    status=$?
    echo "- $target: exit $status, log $(realpath --relative-to="$ROOT" "$log")" >>"$SUMMARY"
done

echo "Results: $OUT"
