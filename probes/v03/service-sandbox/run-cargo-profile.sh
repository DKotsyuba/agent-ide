#!/bin/bash
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ALLOWED_ROOT="$1"; CACHE_ROOT="$2"; RUSTUP_HOME="$3"; CARGO_HOME="$4"; shift 4
if [ "$1" != "--" ]; then echo "expected --" >&2; exit 2; fi
shift
PROFILE="/private/tmp/aiv3-probe/profile-cargo.$$.sb"
trap 'rm -f "$PROFILE"' EXIT
sed -e "s#__ALLOWED_ROOT__#${ALLOWED_ROOT}#g" \
    -e "s#__CACHE_ROOT__#${CACHE_ROOT}#g" \
    -e "s#__RUSTUP_HOME__#${RUSTUP_HOME}#g" \
    -e "s#__CARGO_HOME__#${CARGO_HOME}#g" \
    "$DIR/profile-cargo.sb.tmpl" > "$PROFILE"
set +e
sandbox-exec -f "$PROFILE" "$@"
status=$?
set -e
exit $status
