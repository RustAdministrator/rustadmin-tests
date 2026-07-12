#!/usr/bin/env bash
set -u

TESTS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$TESTS_ROOT/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$TESTS_ROOT/results/$STAMP/advisories"
SUMMARY="$TESTS_ROOT/results/$STAMP/summary.md"
mkdir -p "$OUT"

run_capture() {
    local name="$1"
    shift
    {
        echo "\$ $*"
        "$@"
    } >"$OUT/$name.log" 2>&1
    local status=$?
    echo "- $name: exit $status" >>"$SUMMARY"
    return 0
}

run_capture_in_dir() {
    local name="$1"
    local dir="$2"
    shift 2
    {
        echo "\$ (cd $dir && $*)"
        (
            cd "$dir" || exit 127
            "$@"
        )
    } >"$OUT/$name.log" 2>&1
    local status=$?
    echo "- $name: exit $status" >>"$SUMMARY"
    return 0
}

note() {
    echo "$*" | tee -a "$OUT/notes.log" >/dev/null
}

{
    echo "# Safety Test Summary - $STAMP"
    echo
    echo "## Advisory Checks"
} >"$SUMMARY"

for repo in hbb_common rustdesk-client rustadmin-server; do
    if [ -d "$ROOT/$repo" ]; then
        run_capture "${repo}_cargo_tree_d" cargo tree --manifest-path "$ROOT/$repo/Cargo.toml" --locked -d
        run_capture "${repo}_cargo_metadata_locked" cargo metadata --manifest-path "$ROOT/$repo/Cargo.toml" --format-version=1 --locked

        if command -v cargo-audit >/dev/null 2>&1; then
            run_capture "${repo}_cargo_audit" cargo audit --file "$ROOT/$repo/Cargo.lock"
        else
            note "cargo-audit missing; install with: cargo install cargo-audit"
            echo "- ${repo}_cargo_audit: skipped, cargo-audit missing" >>"$SUMMARY"
        fi

        if command -v cargo-deny >/dev/null 2>&1; then
            run_capture "${repo}_cargo_deny" cargo deny --manifest-path "$ROOT/$repo/Cargo.toml" --locked check
        else
            note "cargo-deny missing; install with: cargo install cargo-deny"
            echo "- ${repo}_cargo_deny: skipped, cargo-deny missing" >>"$SUMMARY"
        fi
    fi
done

if [ "${RUN_ONLINE:-0}" = "1" ]; then
    if command -v osv-scanner >/dev/null 2>&1; then
        run_capture "osv_scanner" osv-scanner --lockfile "$ROOT/hbb_common/Cargo.lock" --lockfile "$ROOT/rustdesk-client/Cargo.lock" --lockfile "$ROOT/rustadmin-server/Cargo.lock"
    else
        note "osv-scanner missing; see https://google.github.io/osv-scanner/"
        echo "- osv_scanner: skipped, osv-scanner missing" >>"$SUMMARY"
    fi

    if command -v npm >/dev/null 2>&1 && [ -d "$ROOT/rustadmin-server/ui/html" ]; then
        if [ -f "$ROOT/rustadmin-server/ui/html/package-lock.json" ] || [ -f "$ROOT/rustadmin-server/ui/html/npm-shrinkwrap.json" ]; then
            run_capture "server_ui_npm_audit" npm --prefix "$ROOT/rustadmin-server/ui/html" audit --json
        else
            note "server UI npm audit skipped; package-lock.json/npm-shrinkwrap.json is missing"
            echo "- server_ui_npm_audit: skipped, no npm lockfile" >>"$SUMMARY"
        fi
    fi

    if command -v flutter >/dev/null 2>&1 && [ -d "$ROOT/rustdesk-client/flutter" ]; then
        run_capture_in_dir "client_flutter_pub_outdated" "$ROOT/rustdesk-client/flutter" flutter pub outdated
    fi
else
    echo "- online checks: skipped; set RUN_ONLINE=1 to enable npm/flutter/OSV network checks" >>"$SUMMARY"
fi

echo "Results: $OUT"
