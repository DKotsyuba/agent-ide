#!/bin/sh
# Regression check: test fixtures own and remove their whole temporary state.
#
# Runs `cargo test` with the given arguments under a private, empty TMPDIR and fails when
# anything is left in it afterwards. Rust's `std::env::temp_dir()` honors TMPDIR, so every
# fixture that writes below it is measured; the compiler's own `xcrun_db` cache is ignored.
#
#   scripts/check-test-tmp.sh -p agent-ide-core --lib telemetry::
#   scripts/check-test-tmp.sh --test project_checks_contract_scheduler
set -eu

scratch=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-tmpcheck.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT INT TERM

# A failing test run is still measured: its fixtures must clean up after a failure too.
status=0
TMPDIR=$scratch/ cargo test "$@" || status=$?

left=$(find "$scratch" -mindepth 1 -maxdepth 1 ! -name xcrun_db | sort)
if [ -n "$left" ]; then
    printf 'fixtures left files in TMPDIR:\n%s\n' "$left" >&2
    exit 1
fi
exit "$status"
