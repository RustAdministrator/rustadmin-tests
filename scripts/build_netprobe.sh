#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST="$ROOT/rustdesk-tests/netprobe/Cargo.toml"

cargo build --release --manifest-path "$MANIFEST" "$@"

BIN="$ROOT/rustdesk-tests/target/release/rustadmin-netprobe"
if [ -f "${BIN}.exe" ]; then
    BIN="${BIN}.exe"
fi

echo "Built: $(realpath --relative-to="$ROOT" "$BIN")"
