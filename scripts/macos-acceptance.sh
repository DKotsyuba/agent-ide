#!/bin/sh
# Runs the bounded macOS product gates or prepares isolated worktrees for one real host driver.

set -eu

ACCEPTANCE_ROUTE=product
ACCEPTANCE_EVIDENCE=
ACCEPTANCE_DRIVER=
ACCEPTANCE_SCHEMA=agent-ide.macos-acceptance.v1
ACCEPTANCE_MAX_EVIDENCE_BYTES=16384
ACCEPTANCE_TMP=
ACCEPTANCE_LEFT=
ACCEPTANCE_RIGHT=
ACCEPTANCE_GO_VERSION=not_tested
ACCEPTANCE_GOPLS_VERSION=not_tested
ACCEPTANCE_RUST_VERSION=not_tested
ACCEPTANCE_RUST_ANALYZER_VERSION=not_tested
ACCEPTANCE_NODE_VERSION=not_tested
ACCEPTANCE_PYRIGHT_VERSION=not_tested
ACCEPTANCE_TYPESCRIPT_LANGUAGE_SERVER_VERSION=not_tested
ACCEPTANCE_TYPESCRIPT_VERSION=not_tested

# Prints the closed command-line interface. It performs no filesystem or process mutation.
usage() {
    printf '%s\n' \
        'usage: scripts/macos-acceptance.sh --evidence ABSOLUTE_PATH [--route product|codex|claude|agent-run-claude|agent-run-codex] [--driver ABSOLUTE_EXECUTABLE]'
}

# Rejects an empty, oversized, or non-public token before it can enter JSON evidence.
#
# The first argument is the token and may contain only letters, digits, dot, underscore, plus, or
# hyphen. The function writes nothing and returns nonzero for values longer than 64 bytes.
safe_token() {
    [ -n "$1" ] && [ "${#1}" -le 64 ] || return 1
    case "$1" in
        *[!A-Za-z0-9._+-]*) return 1 ;;
    esac
}

# Requires one absolute readable regular file, and optionally executable permission.
#
# The first argument names the environment setting for diagnostics, the second is its value, and
# the third is `executable` or `readable`. No path is printed or retained in public evidence.
require_file() {
    case "$2" in
        /*) ;;
        *) printf '%s must be an absolute file\n' "$1" >&2; return 1 ;;
    esac
    [ -f "$2" ] && [ -r "$2" ] || {
        printf '%s is not a readable regular file\n' "$1" >&2
        return 1
    }
    if [ "$3" = executable ] && [ ! -x "$2" ]; then
        printf '%s is not executable\n' "$1" >&2
        return 1
    fi
}

# Requires one absolute readable directory without exposing it in evidence.
#
# The first argument names the environment setting and the second is its directory value. Failure
# is reported only by the public setting name, never by the operator's local path.
require_directory() {
    case "$2" in
        /*) ;;
        *) printf '%s must be an absolute directory\n' "$1" >&2; return 1 ;;
    esac
    [ -d "$2" ] && [ -r "$2" ] || {
        printf '%s is not a readable directory\n' "$1" >&2
        return 1
    }
}

# Removes only the two worktrees and temporary directory created by this process.
#
# Missing or partially created worktrees are ignored so setup failures remain recoverable. The
# repository root and temporary paths are resolved before this function is installed as a trap.
cleanup() {
    if [ -n "$ACCEPTANCE_LEFT" ] && [ -d "$ACCEPTANCE_LEFT" ]; then
        /usr/bin/git -C "$ACCEPTANCE_ROOT" worktree remove --force "$ACCEPTANCE_LEFT" >/dev/null 2>&1 || :
    fi
    if [ -n "$ACCEPTANCE_RIGHT" ] && [ -d "$ACCEPTANCE_RIGHT" ]; then
        /usr/bin/git -C "$ACCEPTANCE_ROOT" worktree remove --force "$ACCEPTANCE_RIGHT" >/dev/null 2>&1 || :
    fi
    if [ -n "$ACCEPTANCE_TMP" ] && [ -d "$ACCEPTANCE_TMP" ]; then
        rm -rf -- "$ACCEPTANCE_TMP"
    fi
}

# Commits one deterministic diagnostic fixture in an already-created detached worktree.
#
# The first argument is the runner-owned worktree and the second is the closed `left` or `right`
# label. It creates only `acceptance-fixture/` and one detached fixture commit; cleanup removes both.
create_fixture() {
    mkdir -p "$1/acceptance-fixture"
    printf '%s\n' \
        'def value() -> int:' \
        "    return \"$2-python-bad\"" \
        >"$1/acceptance-fixture/fixture.py"
    printf '%s\n' \
        "export const value: number = \"$2-typescript-bad\";" \
        'export const use: number = value;' \
        >"$1/acceptance-fixture/fixture.ts"
    printf '%s\n' \
        '{"compilerOptions":{"types":[],"moduleResolution":"node10"},"files":["fixture.ts"]}' \
        >"$1/acceptance-fixture/tsconfig.json"
    /usr/bin/git -C "$1" add -- acceptance-fixture
    /usr/bin/git -C "$1" \
        -c user.name='Acceptance Fixture' \
        -c user.email='acceptance@example.invalid' \
        commit --quiet -m "test: add $2 acceptance fixture"
}

# Verifies every exact external tool path and version consumed by the product gates.
#
# Inputs are the documented `AGENT_IDE_*` environment paths for the accepted release languages
# Rust, Python, and TypeScript/JavaScript; Go and gopls are outside the release scope, are never
# executed or required, and their evidence stays `not_tested`. The function executes only fixed
# version queries and returns nonzero on any mismatch; paths and command output never enter evidence.
verify_toolchains() {
    : "${AGENT_IDE_RUST_ANALYZER:?AGENT_IDE_RUST_ANALYZER is required}"
    : "${AGENT_IDE_RUST_TOOLCHAIN:?AGENT_IDE_RUST_TOOLCHAIN is required}"
    : "${AGENT_IDE_RUST_TOOLCHAIN_DIR:?AGENT_IDE_RUST_TOOLCHAIN_DIR is required}"
    : "${AGENT_IDE_NODE:?AGENT_IDE_NODE is required}"
    : "${AGENT_IDE_PYRIGHT:?AGENT_IDE_PYRIGHT is required}"
    : "${AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER:?AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER is required}"
    : "${AGENT_IDE_TSSERVER:?AGENT_IDE_TSSERVER is required}"

    require_file AGENT_IDE_RUST_ANALYZER "$AGENT_IDE_RUST_ANALYZER" executable || return 1
    require_directory AGENT_IDE_RUST_TOOLCHAIN_DIR "$AGENT_IDE_RUST_TOOLCHAIN_DIR" || return 1
    require_file AGENT_IDE_NODE "$AGENT_IDE_NODE" executable || return 1
    require_file AGENT_IDE_PYRIGHT "$AGENT_IDE_PYRIGHT" executable || return 1
    require_file AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER "$AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER" executable || return 1
    require_file AGENT_IDE_TSSERVER "$AGENT_IDE_TSSERVER" readable || return 1
    require_file rustc "$AGENT_IDE_RUST_TOOLCHAIN_DIR/bin/rustc" executable || return 1
    require_file cargo "$AGENT_IDE_RUST_TOOLCHAIN_DIR/bin/cargo" executable || return 1

    case "$("$AGENT_IDE_RUST_ANALYZER" --version)" in 'rust-analyzer 1.98.1 '*) ;; *) return 1 ;; esac
    case "$("$AGENT_IDE_RUST_TOOLCHAIN_DIR/bin/rustc" --version)" in 'rustc 1.98.1 '*) ;; *) return 1 ;; esac
    [ "$("$AGENT_IDE_NODE" --version)" = v24.4.0 ] || return 1
    [ "$("$AGENT_IDE_NODE" "$AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER" --version)" = 6.0.0 ] || return 1
    ACCEPTANCE_TYPESCRIPT_PACKAGE=$(dirname "$(dirname "$AGENT_IDE_TSSERVER")")/package.json
    require_file typescript "$ACCEPTANCE_TYPESCRIPT_PACKAGE" readable || return 1
    [ "$(jq -r '.version' "$ACCEPTANCE_TYPESCRIPT_PACKAGE")" = 5.9.3 ] || return 1
    ACCEPTANCE_PYRIGHT_CLI=$(dirname "$AGENT_IDE_PYRIGHT")/pyright
    require_file pyright "$ACCEPTANCE_PYRIGHT_CLI" executable || return 1
    [ "$("$ACCEPTANCE_PYRIGHT_CLI" --version)" = 'pyright 1.1.413' ] || return 1
    ACCEPTANCE_RUST_VERSION=1.98.1
    ACCEPTANCE_RUST_ANALYZER_VERSION=1.98.1
    ACCEPTANCE_NODE_VERSION=24.4.0
    ACCEPTANCE_PYRIGHT_VERSION=1.1.413
    ACCEPTANCE_TYPESCRIPT_LANGUAGE_SERVER_VERSION=6.0.0
    ACCEPTANCE_TYPESCRIPT_VERSION=5.9.3
}

# Runs the focused product gates that collectively cover every accepted-language evidence field.
#
# Tests inherit only caller-selected exact tool paths. The preflight refuses a missing or renamed
# test: Cargo returns success for a filtered run with zero matches. Each command is fixed, serial,
# and locked; a first failure cannot become a passing evidence row.
# The Rust gate is the existing cross-crate rust-analyzer proof; the two locked Go/gopls provider
# gates are excluded with the language. `divergent_worktrees` is proven by the locked real
# TypeScript cross-worktree isolation gate, by this runner's verified divergent fixture worktrees,
# and by the exact real-host driver document.
run_product_gates() {
    ACCEPTANCE_TEST_LIST=$(cargo test --locked --test product_mcp_contract -- --list) || return 1
    for ACCEPTANCE_GATE in \
        configured_product_rust_resolves_definition_across_a_crate_boundary \
        configured_product_isolates_typescript_across_two_divergent_worktree_actors \
        configured_product_returns_real_typescript_family_context_and_reaps \
        configured_product_returns_real_pyright_semantic_context_and_reaps \
        configured_product_acceptance_edit_diagnostics_telemetry_and_fallback \
        configured_product_claude_helper_returns_real_pyright_semantic_context_diff_and_stop \
        configured_product_claude_helper_returns_real_typescript_semantic_context_and_reaps
    do
        printf '%s\n' "$ACCEPTANCE_TEST_LIST" | grep -Fxq "$ACCEPTANCE_GATE: test" || return 1
        cargo test --locked --test product_mcp_contract "$ACCEPTANCE_GATE" -- --ignored --exact --nocapture --test-threads=1 || return 1
    done
    verify_toolchains || return 1
}

# Executes one explicitly supplied real-host driver against the two isolated fixture worktrees.
#
# The driver receives route, left/right worktree, and result-file paths through environment only and
# receives no arguments or run identifier. Success requires the exact fixed public result document;
# arbitrary output, missing scenarios, reordered fields, and private metadata are rejected.
run_external_driver() {
    require_file driver "$ACCEPTANCE_DRIVER" executable || return 1
    ACCEPTANCE_RESULT="$ACCEPTANCE_TMP/host-result"
    AGENT_IDE_ACCEPTANCE_ROUTE="$ACCEPTANCE_ROUTE" \
    AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE="$ACCEPTANCE_LEFT" \
    AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE="$ACCEPTANCE_RIGHT" \
    AGENT_IDE_ACCEPTANCE_RESULT="$ACCEPTANCE_RESULT" \
        "$ACCEPTANCE_DRIVER" || return 1
    [ -f "$ACCEPTANCE_RESULT" ] || return 1
    ACCEPTANCE_EXPECTED='schema=agent-ide.host-cell.v1
edit_diagnostic_loop=real_pass
stale_edit_zero_write=real_pass
native_fallback=real_pass
telemetry_restart_query_export=real_pass
compact_content=real_pass
python_provider=real_pass
typescript_r3=real_pass
divergent_worktrees=real_pass'
    [ "$(sed -n '1,10p' "$ACCEPTANCE_RESULT")" = "$ACCEPTANCE_EXPECTED" ] || return 1
    [ "$(wc -l <"$ACCEPTANCE_RESULT" | tr -d ' ')" -le 9 ] || return 1
    verify_toolchains || return 1
}

# Writes one closed JSON evidence object without paths, diagnostics, commands, or private IDs.
#
# The first argument is the overall closed status and the second is the status applied to all eight
# independently named scenarios. Remaining values come from already validated globals. The file is
# created only when absent and is rejected if its final UTF-8 size exceeds 16 KiB.
write_evidence() {
    [ ! -e "$ACCEPTANCE_EVIDENCE" ] || {
        printf '%s\n' 'evidence target already exists' >&2
        return 1
    }
    umask 077
    printf '%s\n' \
        '{' \
        "  \"schema\": \"$ACCEPTANCE_SCHEMA\"," \
        "  \"revision\": \"$ACCEPTANCE_REVISION\"," \
        "  \"route\": \"$ACCEPTANCE_ROUTE_JSON\"," \
        "  \"status\": \"$1\"," \
        "  \"platform\": {\"os\": \"$ACCEPTANCE_OS\", \"version\": \"$ACCEPTANCE_OS_VERSION\", \"architecture\": \"$ACCEPTANCE_ARCH\"}," \
        "  \"host\": {\"version\": \"$ACCEPTANCE_HOST_VERSION\"}," \
        "  \"toolchains\": {\"go\": \"$ACCEPTANCE_GO_VERSION\", \"gopls\": \"$ACCEPTANCE_GOPLS_VERSION\", \"rust\": \"$ACCEPTANCE_RUST_VERSION\", \"rust_analyzer\": \"$ACCEPTANCE_RUST_ANALYZER_VERSION\", \"node\": \"$ACCEPTANCE_NODE_VERSION\", \"pyright\": \"$ACCEPTANCE_PYRIGHT_VERSION\", \"typescript_language_server\": \"$ACCEPTANCE_TYPESCRIPT_LANGUAGE_SERVER_VERSION\", \"typescript\": \"$ACCEPTANCE_TYPESCRIPT_VERSION\"}," \
        "  \"scenarios\": {\"edit_diagnostic_loop\": \"$2\", \"stale_edit_zero_write\": \"$2\", \"native_fallback\": \"$2\", \"telemetry_restart_query_export\": \"$2\", \"compact_content\": \"$2\", \"python_provider\": \"$2\", \"typescript_r3\": \"$2\", \"divergent_worktrees\": \"$2\"}," \
        '  "privacy": {"source": false, "prompts": false, "credentials": false, "commands": false, "paths": false, "diagnostic_messages": false, "private_ids": false}' \
        '}' >"$ACCEPTANCE_EVIDENCE"
    [ "$(wc -c <"$ACCEPTANCE_EVIDENCE" | tr -d ' ')" -le "$ACCEPTANCE_MAX_EVIDENCE_BYTES" ]
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --route)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            ACCEPTANCE_ROUTE=$2
            shift 2
            ;;
        --evidence)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            ACCEPTANCE_EVIDENCE=$2
            shift 2
            ;;
        --driver)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            ACCEPTANCE_DRIVER=$2
            shift 2
            ;;
        --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

case "$ACCEPTANCE_ROUTE" in
    product) ACCEPTANCE_ROUTE_JSON=product ;;
    codex) ACCEPTANCE_ROUTE_JSON=codex ;;
    claude) ACCEPTANCE_ROUTE_JSON=claude ;;
    agent-run-claude) ACCEPTANCE_ROUTE_JSON=agent_run_claude ;;
    agent-run-codex) ACCEPTANCE_ROUTE_JSON=agent_run_codex ;;
    *) usage >&2; exit 2 ;;
esac
case "$ACCEPTANCE_EVIDENCE" in
    /*) ;;
    *) printf '%s\n' '--evidence must be an absolute new path' >&2; exit 2 ;;
esac
[ -d "$(dirname "$ACCEPTANCE_EVIDENCE")" ] || {
    printf '%s\n' 'evidence parent must already exist' >&2
    exit 2
}

ACCEPTANCE_ROOT=$(/usr/bin/git rev-parse --show-toplevel)
ACCEPTANCE_REVISION=$(/usr/bin/git -C "$ACCEPTANCE_ROOT" rev-parse HEAD)
case "$ACCEPTANCE_REVISION" in
    *[!0-9a-f]*|'') printf '%s\n' 'revision is not a full Git object ID' >&2; exit 1 ;;
esac
[ "${#ACCEPTANCE_REVISION}" -eq 40 ] || exit 1

case "$(/usr/bin/uname -s)" in
    Darwin) ACCEPTANCE_OS=macos ;;
    Linux)
        ACCEPTANCE_OS=linux
        ACCEPTANCE_OS_VERSION=not_tested
        ACCEPTANCE_ARCH=other
        ACCEPTANCE_HOST_VERSION=not_tested
        write_evidence not_tested not_tested
        exit 0
        ;;
    *)
        ACCEPTANCE_OS=other
        ACCEPTANCE_OS_VERSION=not_tested
        ACCEPTANCE_ARCH=other
        ACCEPTANCE_HOST_VERSION=not_tested
        write_evidence not_tested not_tested
        exit 0
        ;;
esac

ACCEPTANCE_OS_VERSION=$(/usr/bin/sw_vers -productVersion)
safe_token "$ACCEPTANCE_OS_VERSION" || exit 1
case "$(/usr/bin/uname -m)" in
    arm64) ACCEPTANCE_ARCH=arm64 ;;
    x86_64) ACCEPTANCE_ARCH=x86_64 ;;
    *) ACCEPTANCE_ARCH=other ;;
esac
if [ "$ACCEPTANCE_ARCH" != arm64 ]; then
    ACCEPTANCE_HOST_VERSION=not_tested
    write_evidence not_tested not_tested
    exit 0
fi

ACCEPTANCE_TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-acceptance.XXXXXX")
ACCEPTANCE_LEFT="$ACCEPTANCE_TMP/left"
ACCEPTANCE_RIGHT="$ACCEPTANCE_TMP/right"
trap cleanup EXIT HUP INT TERM
/usr/bin/git -C "$ACCEPTANCE_ROOT" worktree add --quiet --detach "$ACCEPTANCE_LEFT" "$ACCEPTANCE_REVISION"
/usr/bin/git -C "$ACCEPTANCE_ROOT" worktree add --quiet --detach "$ACCEPTANCE_RIGHT" "$ACCEPTANCE_REVISION"
create_fixture "$ACCEPTANCE_LEFT" left
create_fixture "$ACCEPTANCE_RIGHT" right
[ "$(/usr/bin/git -C "$ACCEPTANCE_LEFT" rev-parse HEAD)" != "$(/usr/bin/git -C "$ACCEPTANCE_RIGHT" rev-parse HEAD)" ]

cd "$ACCEPTANCE_ROOT"
if [ "$ACCEPTANCE_ROUTE" = product ]; then
    [ -z "$ACCEPTANCE_DRIVER" ] || { printf '%s\n' 'product route does not accept a driver' >&2; exit 2; }
    ACCEPTANCE_HOST_VERSION=product-contract
    if run_product_gates; then
        write_evidence product_pass product_pass
    else
        write_evidence failed failed
        exit 1
    fi
elif [ -z "$ACCEPTANCE_DRIVER" ]; then
    ACCEPTANCE_HOST_VERSION=not_tested
    write_evidence not_tested not_tested
else
    : "${AGENT_IDE_ACCEPTANCE_HOST_VERSION:?AGENT_IDE_ACCEPTANCE_HOST_VERSION is required with --driver}"
    safe_token "$AGENT_IDE_ACCEPTANCE_HOST_VERSION" || exit 2
    ACCEPTANCE_HOST_VERSION=$AGENT_IDE_ACCEPTANCE_HOST_VERSION
    if run_external_driver; then
        write_evidence real_pass real_pass
    else
        write_evidence failed failed
        exit 1
    fi
fi
