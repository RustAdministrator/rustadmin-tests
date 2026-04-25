# RustDesk Code Quality Review Notes - Safety Tests

Generated: 2026-04-25

Scope: testability, dependency hygiene, and maintainability issues surfaced while building and running the standalone safety harness. This is not a complete static review of all RustDesk source files.

## Findings

### QUAL-001: Security behavior needs executable regression tests

The original repos did not contain a fuzz suite for the reviewed parser/codec surfaces. The new `rustdesk-tests` harness adds defensive coverage without changing production code.

Recommendations:
- Keep security probes outside production code until each behavior is hardened.
- After hardening, promote the smallest stable regression tests into the owning crate.
- Keep `EXPECT_HARDENED=1` jobs in CI for known unsafe behaviors once fixed.

### QUAL-002: Server dependency graph is not lockfile-reproducible

`cargo metadata --locked` fails for `rustdesk-server`.

Recommendations:
- Fix lockfile reproducibility before using dependency advisory results as release gates.
- Require locked metadata in CI and release scripts.
- Review any generated lockfile change as a supply-chain change, not a mechanical formatting change.

### QUAL-003: `cargo-deny` is running without a project policy

No `deny.toml` was found, so the default policy produces broad license failures and is not yet actionable enough for CI.

Recommendations:
- Add an explicit `deny.toml`.
- Define accepted licenses, allowed git sources, duplicate crate policy, and advisory exception expiry dates.
- Keep default-deny for unknown sources and unknown licenses.

### QUAL-004: Server UI dependency graph is unlocked

`npm audit` cannot run because the server UI has no npm lockfile.

Recommendations:
- Commit a package lock for any UI that is built or shipped.
- If the UI is intentionally not reproducible, document that and keep it out of release-critical builds.

### QUAL-005: Flutter dependency drift is high

`flutter pub outdated` found 40 dependencies constrained below a resolvable newer version and discontinued packages `js`, `build_resolvers`, and `build_runner_core`.

Recommendations:
- Separate runtime dependency upgrades from generator/tooling upgrades.
- Prioritize discontinued packages and platform plugins with security-sensitive permissions.
- Add a scheduled dependency drift report.

### QUAL-006: Fuzz and dynamic tests need corpus ownership

The initial fuzz smoke passed, but five seconds per target only validates harness health.

Recommendations:
- Add sanitized seed corpora for frame codec, rendezvous messages, generic messages, file paths, and compressed payloads.
- Store crash artifacts and minimization output under ignored result directories.
- Use longer scheduled fuzz runs and short PR smoke runs.
