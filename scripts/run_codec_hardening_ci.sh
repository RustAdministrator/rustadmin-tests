#!/usr/bin/env bash
set -u

TESTS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$TESTS_ROOT/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$TESTS_ROOT/results/$STAMP/codec_hardening_ci"
SUMMARY="$TESTS_ROOT/results/$STAMP/summary.md"
PROBE_MANIFEST="$TESTS_ROOT/dynamic/safety_probes/Cargo.toml"
mkdir -p "$OUT"

{
    echo "# Safety Test Summary - $STAMP"
    echo
    echo "## Codec Hardening CI"
} >"$SUMMARY"

run_capture() {
    local name="$1"
    shift
    local log="$OUT/${name}.log"
    "$@" >"$log" 2>&1
    local status=$?
    echo "- $name: exit $status, log $(realpath --relative-to="$ROOT" "$log")" >>"$SUMMARY"
    return "$status"
}

status=0

run_capture "hbb_common_bytes_codec_tests" \
    cargo test --manifest-path "$ROOT/hbb_common/Cargo.toml" --lib bytes_codec || status=$?

run_capture "codec_header_alloc_probe" \
    env EXPECT_HARDENED=1 cargo run --manifest-path "$PROBE_MANIFEST" --bin codec_header_alloc_probe || status=$?

echo "Results: $OUT"
exit "$status"
