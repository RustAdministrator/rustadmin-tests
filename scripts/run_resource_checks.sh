#!/usr/bin/env bash
set -u

TESTS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$TESTS_ROOT/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$TESTS_ROOT/results/$STAMP/resource"
SUMMARY="$TESTS_ROOT/results/$STAMP/summary.md"
MANIFEST="$TESTS_ROOT/dynamic/safety_probes/Cargo.toml"
mkdir -p "$OUT"

{
    echo "# Safety Test Summary - $STAMP"
    echo
    echo "## Resource Checks"
} >"$SUMMARY"

cargo build --manifest-path "$MANIFEST" --bins >"$OUT/build.log" 2>&1
build_status=$?
echo "- build: exit $build_status, log $(realpath --relative-to="$ROOT" "$OUT/build.log")" >>"$SUMMARY"
if [ "$build_status" -ne 0 ]; then
    echo "Build failed; resource probes skipped"
    exit 0
fi

run_resource() {
    local name="$1"
    shift
    local log="$OUT/${name}.log"
    /usr/bin/time -v "$TESTS_ROOT/target/debug/$name" "$@" >"$log" 2>&1
    local status=$?
    echo "- $name: exit $status, log $(realpath --relative-to="$ROOT" "$log")" >>"$SUMMARY"
}

run_resource codec_header_alloc_probe "${CODEC_ADVERTISED_LEN:-67108864}"
run_resource zstd_expansion_probe "${ZSTD_EXPANSION_BYTES:-8388608}"

echo "Results: $OUT"
