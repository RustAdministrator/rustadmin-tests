#!/bin/sh
set -eu

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
tests_root="$(cd "${script_dir}/.." && pwd)"
workspace_root="$(cd "${tests_root}/.." && pwd)"
codec_root="${1:-${RUSTDESK_MACOS_CODEC_ROOT:-}}"

client_root=""
for candidate in rustdesk-client rustadmin rustadmin-client; do
  if [ -f "${workspace_root}/${candidate}/libs/scrap/Cargo.toml" ]; then
    client_root="${workspace_root}/${candidate}"
    break
  fi
done

if [ -z "${client_root}" ]; then
  echo "error: could not find rustdesk-client, rustadmin, or rustadmin-client under ${workspace_root}" >&2
  exit 1
fi

if [ -n "${codec_root}" ]; then
  export RUSTDESK_MACOS_CODEC_ROOT="${codec_root}"
  export CMAKE_PREFIX_PATH="${codec_root}${CMAKE_PREFIX_PATH:+:${CMAKE_PREFIX_PATH}}"
fi

runtime_prefixes="${codec_root:-${CMAKE_PREFIX_PATH:-}}"
runtime_library_path=""
remaining_prefixes="${runtime_prefixes}"
while [ -n "${remaining_prefixes}" ]; do
  case "${remaining_prefixes}" in
    *:*)
      prefix="${remaining_prefixes%%:*}"
      remaining_prefixes="${remaining_prefixes#*:}"
      ;;
    *)
      prefix="${remaining_prefixes}"
      remaining_prefixes=""
      ;;
  esac
  for lib_dir in "${prefix}/lib" "${prefix}/lib64"; do
    if [ -d "${lib_dir}" ]; then
      runtime_library_path="${runtime_library_path}${runtime_library_path:+:}${lib_dir}"
    fi
  done
done

if [ -n "${runtime_library_path}" ]; then
  export DYLD_LIBRARY_PATH="${runtime_library_path}${DYLD_LIBRARY_PATH:+:${DYLD_LIBRARY_PATH}}"
fi

cargo test \
  --manifest-path "${client_root}/libs/scrap/Cargo.toml" \
  --features hwcodec \
  --lib videotoolbox_high_quality_ \
  --target-dir "${tests_root}/target-scrap-macos" \
  -- \
  --ignored \
  --nocapture \
  --test-threads=1
