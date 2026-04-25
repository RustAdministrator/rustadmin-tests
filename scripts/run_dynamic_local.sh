#!/usr/bin/env bash
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$ROOT/rustdesk-tests/results/$STAMP/dynamic"
SUMMARY="$ROOT/rustdesk-tests/results/$STAMP/summary.md"
MANIFEST="$ROOT/rustdesk-tests/dynamic/safety_probes/Cargo.toml"
mkdir -p "$OUT"

{
    echo "# Safety Test Summary - $STAMP"
    echo
    echo "## Dynamic Local Probes"
} >"$SUMMARY"

run_probe() {
    local name="$1"
    shift
    local log="$OUT/${name}.log"
    cargo run --manifest-path "$MANIFEST" --bin "$name" -- "$@" >"$log" 2>&1
    local status=$?
    echo "- $name: exit $status, log $(realpath --relative-to="$ROOT" "$log")" >>"$SUMMARY"
}

run_probe ipc_path_probe
run_probe codec_header_alloc_probe
run_probe file_transfer_path_probe
run_probe zstd_expansion_probe

if [ -n "${HBBS_ADDR:-}" ]; then
    count="${ONLINE_REQUEST_PEERS:-10000}"
    key="${HBBS_KEY:-}"
    run_probe online_request_client "$HBBS_ADDR" "$count" "$key"
else
    echo "- online_request_client: skipped; set HBBS_ADDR=127.0.0.1:21115 to run against a local test hbbs" >>"$SUMMARY"
fi

echo "Results: $OUT"

