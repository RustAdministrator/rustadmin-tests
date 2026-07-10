#!/usr/bin/env bash
set -euo pipefail

workspace_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
codec_root="${1:-${RUSTDESK_MACOS_CODEC_ROOT:-}}"

if [[ -n "${codec_root}" ]]; then
  export RUSTDESK_MACOS_CODEC_ROOT="${codec_root}"
  export CMAKE_PREFIX_PATH="${codec_root}${CMAKE_PREFIX_PATH:+:${CMAKE_PREFIX_PATH}}"
fi

cargo test \
  --manifest-path "${workspace_root}/rustadmin-client/libs/scrap/Cargo.toml" \
  --features hwcodec \
  --lib videotoolbox_high_quality_ \
  --target-dir "${workspace_root}/rustadmin-tests/target-scrap-macos" \
  -- \
  --ignored \
  --nocapture \
  --test-threads=1
