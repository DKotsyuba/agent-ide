#!/bin/bash
# Renders profile.sb.tmpl with the given allowed root / cache root and runs a command under it.
# Usage: run-profile.sh <allowed_root> <cache_root> -- <command...>
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ALLOWED_ROOT="$1"; shift
CACHE_ROOT="$1"; shift
if [ "$1" != "--" ]; then echo "expected --" >&2; exit 2; fi
shift
PROFILE="/private/tmp/aiv3-probe/profile.$$.sb"
trap 'rm -f "$PROFILE"' EXIT
sed -e "s#__ALLOWED_ROOT__#${ALLOWED_ROOT}#g" -e "s#__CACHE_ROOT__#${CACHE_ROOT}#g" "$DIR/profile.sb.tmpl" > "$PROFILE"
set +e
sandbox-exec -f "$PROFILE" "$@"
status=$?
set -e
exit $status
