# Upstream Integration Test Plan - 2026-05-10

This plan tracks test updates needed when importing the upstream changes
reviewed in `../upstream_review_20260510.md` and planned in
`../upstream_integration_plan_20260510.md`.

The directory is named `rustdesk-tests` in this workspace. It is the safety test
tree referred to as RustAdmin tests in the review discussion.

## Existing Coverage To Keep

- `codec_header_alloc_probe`: protects the default byte-framing allocation cap.
- `file_transfer_path_probe`: protects file-transfer path traversal handling.
- `ipc_path_probe`: baseline probe for unsafe IPC parent behavior.
- `online_request_client`: local-only rendezvous request-load probe.
- Fuzz targets for `bytes_codec_decode`, `rendezvous_message_decode`,
  `message_decode`, `file_transfer_paths`, and `zstd_decompress`.

## Required Updates By Integration Phase

### hbb_common IPC Path Hardening

When upstream `40368d4` is integrated, update or add probes for:

- normal IPC path uses a per-UID parent on Linux/macOS
  (`/tmp/{APP}-{uid}/ipc...` upstream behavior).
- service and uinput IPC use the shared service parent
  (`/tmp/{APP}-service/ipc_service`, `/tmp/{APP}-service/ipc_uinput_*`).
- normal IPC parent is not world-writable.
- pre-existing symlink or unsafe parent path is not chmodded into an unsafe
  target.

Candidate implementation:

- Extend `dynamic/safety_probes/src/bin/ipc_path_probe.rs` after the codebase
  contains the new hbb_common IPC helpers.
- Add `EXPECT_HARDENED=1 ./rustdesk-tests/scripts/run_dynamic_local.sh` to the
  merge checklist.

### Client IPC And Portable Service

When upstream `9df486a68` is integrated, add probes or Rust unit tests for:

- unauthorized local IPC peers are rejected.
- service-scoped IPC allows only root/SYSTEM or active user paths expected by
  upstream logic.
- executable mismatch blocks service IPC where the platform supports peer
  executable lookup.
- portable-service shared-memory names reject empty, overlong, or
  non-alphanumeric names except `_` and `-`.
- portable-service IPC token rejects malformed length/case/characters.
- token compare remains fixed-length and non-early-success.

Preferred location:

- Rust unit tests near `rustdesk-client/src/ipc/auth.rs`,
  `rustdesk-client/src/ipc/fs.rs`, and
  `rustdesk-client/src/server/portable_service.rs`.
- Keep `rustdesk-tests` for black-box/local probes that do not require private
  function access.

### Switch-Side Hardening

When upstream `f29dec7b1` is integrated, test:

- `--switch_uuid` without a matching local pending UUID is ignored.
- a matching UUID can be consumed once only.
- stale pending UUIDs expire.
- switch-back permission is one-shot.

Preferred location:

- Rust unit tests around the pending switch UUID store in
  `rustdesk-client/src/server/connection.rs`.

### Deeplink Gates

When upstream `1e9c4d04f` and hbb_common `ea0ac7c` are integrated, test:

- mobile `rustdesk://config/...` is rejected when
  `allow-deep-link-server-settings` is unset or `N`.
- mobile `rustdesk://password/...` is rejected when
  `allow-deep-link-password` is unset or `N`.
- both paths only proceed after explicit `Y`.
- generic connection deeplinks still work.

Preferred location:

- Flutter tests in `rustdesk-client/flutter/test/`.

### Accept-Window Permission Changes

If upstream `383a5c347` / hbb_common `3e31a94` is imported, test RustAdmin
policy explicitly:

- fresh config: accept-window permission changes are blocked.
- hard setting `enable-perm-change-in-accept-window=N`: blocked.
- explicit operator setting `enable-perm-change-in-accept-window=Y`: allowed.
- blocked UI state does not optimistically flip permission icons.
- backend still rejects permission changes if UI is bypassed.

This is a fork-policy test. It should fail if upstream default-allow behavior is
imported unchanged.

### Privacy Mode Permission

If upstream privacy-mode permission plumbing is imported, test:

- remote privacy-mode toggle fails when permission is disabled.
- revoking privacy-mode permission attempts to leave privacy mode first.
- failure to leave privacy mode rolls UI/backend state back instead of lying to
  the user.
- default policy matches the RustAdmin decision in
  `../upstream_integration_plan_20260510.md`.

### Terminal Reconnect

If upstream terminal reconnect is imported, test:

- terminal replay buffer is capped.
- incomplete UTF-8 chunks are reassembled or safely flushed.
- reconnect does not duplicate already-open terminal tabs.
- terminal remains disabled when `enable-terminal=N`.

Preferred split:

- Rust tests for buffer/UTF-8 helpers in `rustdesk-client/src/server/terminal_service.rs`.
- Flutter tests for tab/reconnect UI behavior if practical.
- Existing fuzz target `message_decode` should continue to cover protobuf
  decode stability for the new terminal field.

## Runner Updates To Consider

After integration, update `scripts/run_dynamic_local.sh` to include any new
standalone probes added under `dynamic/safety_probes/src/bin/`.

Keep CI runners separated:

- `run_codec_hardening_ci.sh`: low-cost gate for framing/resource regressions.
- `run_dynamic_local.sh`: local IPC, path, and localhost-only probes.
- `run_resource_checks.sh`: memory/time-visible resource checks.
- `run_fuzz_smoke.sh`: short parser/fuzzer smoke coverage.

## Merge Checklist

For each integration branch:

- record upstream commit IDs copied or reimplemented.
- record whether the behavior is copied, adapted, or RustAdmin-only.
- update this plan if a test is added or deliberately deferred.
- run the relevant runner script and keep the generated result path in the
  branch handoff.
