#!/bin/sh
# Real-host acceptance driver for the v0.3 project problem feed (EYES-r2) on direct Claude Code.
#
# Exercises the candidate `agent-ide` binary's managed-Claude MCP + plugin-hook path against real
# `claude -p` sessions and real pinned Rust/Python toolchains, over disposable fixture git repos
# copied from tests/fixtures/eyes/. Every scenario is a bounded, real, non-interactive Claude
# session (`--output-format json`); the model's own final answer is the recorded evidence, since
# plain `json` output carries no per-tool-call transcript. At most 8 real Claude sessions run
# across one invocation of `./eyes-claude.sh all`; each scenario can also be run standalone by name.
#
# Usage: ./eyes-claude.sh <scenario|all>
#   scenario in: rust python-venv python-noenv worktree-a worktree-b idle confinement all
#
# Operator environment (all optional):
#   AGENT_IDE_ACCEPTANCE_BINARY        candidate executable; default <repo>/target/release/agent-ide
#   AGENT_IDE_ACCEPTANCE_CLAUDE        real Claude Code CLI; default /Users/pluto/.local/bin/claude
#   AGENT_IDE_ACCEPTANCE_MODEL         economical model alias; default haiku
#   AGENT_IDE_ACCEPTANCE_OPERATOR_HOME operator home with the existing Claude login; default /Users/pluto
#   AGENT_IDE_ACCEPTANCE_WORKDIR       scratch root for fixture copies; default /private/tmp/aiv3-acc-$$
#   AGENT_IDE_RUST_TOOLCHAIN_DIR       accepted Rust 1.98.1 toolchain dir
#   AGENT_IDE_NODE                     accepted Node 24.4.0 binary
#   AGENT_IDE_PYRIGHT                  accepted Pyright 1.1.413 CLI symlink (its resolved target is
#                                       the entry module passed to the daemon as `pyright_cli`)
#
# Every failure records a private diagnostic line and this driver's own stdout; nothing here writes
# to the owner's real ~/.claude/settings.json, ~/.config/agent-ide, or ~/.local/bin/agent-ide, and no
# process this driver did not itself start is ever signaled.

set -eu

DRIVER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DRIVER_ROOT=$(CDPATH= cd -- "$DRIVER_DIR/../.." && pwd)
. "$DRIVER_DIR/driver-common.sh"

BINARY=${AGENT_IDE_ACCEPTANCE_BINARY:-$DRIVER_ROOT/target/release/agent-ide}
CLAUDE=${AGENT_IDE_ACCEPTANCE_CLAUDE:-/Users/pluto/.local/bin/claude}
MODEL=${AGENT_IDE_ACCEPTANCE_MODEL:-haiku}
OPERATOR_HOME=${AGENT_IDE_ACCEPTANCE_OPERATOR_HOME:-/Users/pluto}
WORKDIR=${AGENT_IDE_ACCEPTANCE_WORKDIR:-/private/tmp/aiv3-acc-$$}

RUST_TOOLCHAIN_DIR=${AGENT_IDE_RUST_TOOLCHAIN_DIR:-/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin}
NODE=${AGENT_IDE_NODE:-/Users/pluto/.nvm/versions/node/v24.4.0/bin/node}
PYRIGHT=${AGENT_IDE_PYRIGHT:-/opt/homebrew/bin/pyright}

[ -x "$BINARY" ] || fail E_BINARY_MISSING "$BINARY"
[ -x "$CLAUDE" ] || fail E_CLAUDE_MISSING "$CLAUDE"
[ -d "$RUST_TOOLCHAIN_DIR" ] || fail E_RUST_TOOLCHAIN_MISSING "$RUST_TOOLCHAIN_DIR"
[ -x "$NODE" ] || fail E_NODE_MISSING "$NODE"
[ -x "$PYRIGHT" ] || fail E_PYRIGHT_MISSING "$PYRIGHT"
PYRIGHT_CLI=$(python3 -c "import os,sys; print(os.path.realpath(sys.argv[1]))" "$PYRIGHT")
[ -f "$PYRIGHT_CLI" ] || fail E_PYRIGHT_CLI_MISSING "$PYRIGHT_CLI"

FIXTURES=$DRIVER_ROOT/tests/fixtures/eyes
[ -d "$FIXTURES/rust-workspace" ] || fail E_RUST_FIXTURE_MISSING
[ -d "$FIXTURES/python-pkg" ] || fail E_PYTHON_FIXTURE_MISSING

mkdir -p -- "$WORKDIR"
DIAG_LOG=$WORKDIR/driver.log
DIAG_DIR=$WORKDIR
: >"$DIAG_LOG"

note eyes-driver-start "workdir=$WORKDIR binary=$BINARY"

# ---------------------------------------------------------------------------
# Launcher-template plumbing.
# ---------------------------------------------------------------------------

# Prints the exact ai-r-<hash> runtime directory for one real git fixture repo.
#
# The key is the repository's canonical git common directory (EYES-r1 §2); the identity is BLAKE3
# of that path's raw bytes, matched here by measuring a private temp file holding exactly those
# bytes (the same technique `project_identity` in driver-common.sh already uses for a candidate).
rendezvous_runtime_dir() {
    common_dir=$(/usr/bin/git -C "$1" rev-parse --path-format=absolute --git-common-dir) \
        || fail E_GIT_COMMON_DIR "$1"
    canonical=$(canonical_dir "$common_dir") || fail E_GIT_COMMON_DIR_CANONICAL "$1"
    identity=$(project_identity "$BINARY" "$canonical") || fail E_RENDEZVOUS_IDENTITY "$1"
    [ "${#identity}" = 64 ] || fail E_RENDEZVOUS_IDENTITY_LENGTH "$1"
    printf '/private/tmp/ai-r-%s' "$(printf '%s' "$identity" | cut -c1-16)"
}

# Writes the shared `--claude-launcher-template` every scenario session uses.
#
# One placeholder target (`bind_one_candidate` overwrites its `attachment`/`candidate` per session,
# EYES-r2 §1); this driver never exercises the v0.2 Go/TypeScript providers. `operation_ms` is generously
# above the v0.2 unit-test default (1000 ms): a real model needs several real seconds between a
# `pending` reply and its own follow-up `ide.inspect` call, and the whole pending ticket expires
# at `operation_ms` after mint (`src/assistance/assembly.rs`).
write_launcher_template() {
    template=$1
    allowed_root=$2
    git_ev=$("$BINARY" evidence executable --identity accepted-git /usr/bin/git) \
        || fail E_EVIDENCE_GIT
    python3 - "$template" "$allowed_root" "$git_ev" \
        "$RUST_TOOLCHAIN_DIR" "$NODE" "$PYRIGHT_CLI" <<'PYEOF'
import json, sys
out_path, allowed_root, git_ev, rust_dir, node, pyright_cli = sys.argv[1:7]
git_ev = json.loads(git_ev)
config = {
    "version": 1,
    "limits": {"queued": 4, "details": 8, "operation_ms": 120000, "output_bytes": 4096},
    "targets": [
        {
            "attachment": "placeholder-attachment",
            "candidate": "/placeholder/candidate",
            "git": git_ev,
            "providers": [],
            "claude_profile": {
                "enabled": True,
                "fail_if_unavailable": True,
                "allow_unsandboxed_commands": False,
                "no_matching_excluded_commands": True,
                "scope_declared": True,
                "platform": "mac_os",
            },
        }
    ],
    "allowed_roots": [allowed_root],
    "project_checks": {
        "debounce_ms": 100,
        "idle_timeout_s": 30,
        "check_timeout_s": 60,
        "rust": {"toolchain_dir": rust_dir},
        "python": {"node": node, "pyright_cli": pyright_cli},
    },
}
with open(out_path, "w") as f:
    json.dump(config, f)
PYEOF
    "$BINARY" launcher check "$template" || fail E_LAUNCHER_INVALID "$template"
}

LAUNCHER=$WORKDIR/launcher.json
write_launcher_template "$LAUNCHER" "$WORKDIR"

MCP_CONFIG=$WORKDIR/mcp-config.json
python3 -c "
import json, sys
binary, launcher, out = sys.argv[1:4]
json.dump({'mcpServers': {'agent-ide': {'type': 'stdio', 'command': binary,
    'args': ['mcp', '--claude-launcher-template', launcher]}}}, open(out, 'w'))
" "$BINARY" "$LAUNCHER" "$MCP_CONFIG"

# ---------------------------------------------------------------------------
# Fixture setup.
# ---------------------------------------------------------------------------

# Copies one tracked fixture tree into a fresh disposable git repository.
copy_fixture_repo() {
    src=$1
    dst=$2
    rm -rf "$dst"
    mkdir -p -- "$(dirname -- "$dst")"
    cp -R "$src" "$dst"
    ( cd "$dst" \
        && /usr/bin/git init -q \
        && /usr/bin/git -c commit.gpgsign=false -c user.name=acceptance \
            -c user.email=acceptance@example.invalid add -A \
        && /usr/bin/git -c commit.gpgsign=false -c user.name=acceptance \
            -c user.email=acceptance@example.invalid commit -q -m "test: fixture baseline" \
    ) || fail E_FIXTURE_COMMIT "$dst"
}

# ---------------------------------------------------------------------------
# Session runner.
# ---------------------------------------------------------------------------

# Runs one bounded real `claude -p` session inside `worktree`, writing the `--output-format json`
# result to `$WORKDIR/result-<label>.json`. Mirrors the env-scrub discipline of the v0.2 drivers:
# PATH reduced to system directories (no ambient `agent-ide` double-observing hooks), the operator's
# real HOME for the existing login, and every nested-session/Anthropic credential variable removed.
run_claude_session() {
    label=$1
    worktree=$2
    prompt=$3
    max_turns=$4
    result=$WORKDIR/result-$label.json
    note "session-$label-start" "$worktree"
    ( cd "$worktree" \
        && HOME="$OPERATOR_HOME" AGENT_IDE_BIN="$BINARY" CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0 \
           PATH=/usr/bin:/bin:/usr/sbin:/sbin \
           /usr/bin/env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL -u ANTHROPIC_API_KEY \
             -u ANTHROPIC_MODEL -u ANTHROPIC_SMALL_FAST_MODEL -u CLAUDECODE \
             -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_EXECPATH \
             -u CLAUDE_CODE_MESSAGING_SOCKET -u CLAUDE_CODE_MESSAGING_TOKEN \
             -u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID -u CLAUDE_EFFORT \
           "$CLAUDE" -p "$(cat "$prompt")" \
             --model "$MODEL" --output-format json --max-turns "$max_turns" \
             --dangerously-skip-permissions --strict-mcp-config --mcp-config "$MCP_CONFIG" \
             --plugin-dir "$DRIVER_ROOT" \
    ) >"$result" 2>"$WORKDIR/stderr-$label.log"
    status=$?
    note "session-$label-exit" "$status"
    [ -s "$result" ] || fail "E_SESSION_${label}_EMPTY"
    return 0
}

# Prints the model's final `result` text of one recorded session.
session_result_text() {
    python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('result',''))" "$WORKDIR/result-$1.json"
}

# ---------------------------------------------------------------------------
# Scenario 1: Rust workspace edit/block/problems/no-repeat.
# ---------------------------------------------------------------------------
scenario_rust() {
    repo=$WORKDIR/rust1
    copy_fixture_repo "$FIXTURES/rust-workspace" "$repo"
    prompt=$WORKDIR/prompt-rust.txt
    cat >"$prompt" <<'EOF'
Work inside this Rust cargo workspace. Follow these steps in order and do not skip any.
IMPORTANT: do not call ide.context or any other ide.* tool between step 3 and step 6 - use only
Bash and your native Edit tool in that range.
1. Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with
   the same detail_ref to get the final outcome.
2. Run Bash: sleep 3
3. Using your native Edit tool (not any ide.* tool), edit crates/a/src/lib.rs so `combine` takes
   three i32 parameters (x, y, z) and returns their sum, instead of two. Do not open or edit
   crates/b/src/lib.rs.
4. Run Bash: echo capture1 -- note the exact text of any <agent-ide> block visible in your context
   right after this call, or NONE if there is none.
5. If step 4 showed NONE, run Bash: sleep 3, then Bash: echo capture2, and note its block the same
   way. If that is still NONE, repeat once more with Bash: sleep 3 then Bash: echo capture3. Stop
   as soon as one of these capture calls shows a real <agent-ide> block. Still do not call any ide.*
   tool in this range.
6. Call ide.context with {"kind":"problems"}. Report the problem it returns for crates/b.
7. Run Bash: echo done
   Capture the exact text of any <agent-ide> block visible in your context right after this step.
Finally, reply with exactly these five lines and nothing else:
START: <ide.start/ide.inspect final outcome text, one line, or ERROR: <what failed>>
POST_EDIT_BLOCK: <exact <agent-ide> block text from the last capture attempt in steps 4-5, or NONE>
PROBLEMS: <the crates/b problem line(s) ide.context returned, or NONE>
NOOP_BLOCK: <exact <agent-ide> block text from step 7, or NONE>
TOOLS_CALLED: <comma separated list of every tool name you called, in order>
EOF
    run_claude_session rust "$repo" "$prompt" 30
    session_result_text rust
}

# ---------------------------------------------------------------------------
# Scenario 2a/2b: Python with and without a real .venv.
# ---------------------------------------------------------------------------
scenario_python_venv() {
    repo=$WORKDIR/py-venv
    copy_fixture_repo "$FIXTURES/python-pkg" "$repo"
    ( cd "$repo" && "$OPERATOR_HOME/.local/bin/uv" venv --managed-python --python 3.14 .venv \
        >>"$DIAG_LOG" 2>&1 ) || fail E_UV_VENV "$repo"
    prompt=$WORKDIR/prompt-py-venv.txt
    cat >"$prompt" <<'EOF'
Work inside this Python package, which has a real .venv already created. Follow these
steps in order and do not skip any. IMPORTANT: do not call ide.context or any other ide.* tool
between step 3 and step 6 - use only Bash and your native Edit tool in that range.
1. Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with
   the same detail_ref to get the final outcome.
2. Run Bash: sleep 3
3. Using your native Edit tool (not any ide.* tool), edit pkg/a.py so `combine` takes three int
   parameters (x, y, z) and returns their sum, instead of two. Do not open or edit pkg/b.py.
4. Run Bash: echo capture1 -- note the exact text of any <agent-ide> block visible in your context
   right after this call, or NONE if there is none.
5. If step 4 showed NONE, run Bash: sleep 3, then Bash: echo capture2, and note its block the same
   way. If that is still NONE, repeat once more with Bash: sleep 3 then Bash: echo capture3. Stop
   as soon as one of these capture calls shows a real <agent-ide> block. Still do not call any ide.*
   tool in this range.
6. Call ide.context with {"kind":"problems"}. Report the problem it returns for pkg/b.py.
7. Run Bash: echo done
   Capture the exact text of any <agent-ide> block visible in your context right after this step.
Finally, reply with exactly these five lines and nothing else:
START: <ide.start/ide.inspect final outcome text, one line, or ERROR: <what failed>>
POST_EDIT_BLOCK: <exact <agent-ide> block text from the last capture attempt in steps 4-5, or NONE>
PROBLEMS: <the pkg/b.py problem line(s) ide.context returned, or NONE>
NOOP_BLOCK: <exact <agent-ide> block text from step 7, or NONE>
TOOLS_CALLED: <comma separated list of every tool name you called, in order>
EOF
    run_claude_session py-venv "$repo" "$prompt" 30
    session_result_text py-venv
}

scenario_python_noenv() {
    repo=$WORKDIR/py-venv
    [ -d "$repo" ] || fail E_PY_NOENV_PRECONDITION "run scenario python-venv first"
    rm -rf "$repo/.venv"
    prompt=$WORKDIR/prompt-py-noenv.txt
    cat >"$prompt" <<'EOF'
Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with the
same detail_ref. Then run Bash: sleep 3. Then run Bash: echo settle. Then call ide.context
with {"kind":"problems","language":"python"}.
Finally, reply with exactly these two lines and nothing else:
START: <ide.start/ide.inspect final outcome text, or ERROR: <what failed>>
PROBLEMS: <the exact python state/text ide.context returned>
EOF
    run_claude_session py-noenv "$repo" "$prompt" 20
    session_result_text py-noenv
}

# ---------------------------------------------------------------------------
# Scenario 3: two worktrees of one Rust repository share one daemon.
# ---------------------------------------------------------------------------
scenario_worktree_a() {
    origin=$WORKDIR/rust-origin
    copy_fixture_repo "$FIXTURES/rust-workspace" "$origin"
    wt_a=$WORKDIR/rust-wt-a
    rm -rf "$wt_a"
    ( cd "$origin" && /usr/bin/git worktree add -q -b wt-a "$wt_a" ) || fail E_WORKTREE_A
    prompt=$WORKDIR/prompt-wt-a.txt
    cat >"$prompt" <<'EOF'
Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with the
same detail_ref. Then run Bash: sleep 4. Then call ide.context with {"kind":"problems"} and
note how long the rust check took (duration_ms, or similar) if reported.
Finally, reply with exactly these two lines and nothing else:
START: <ide.start/ide.inspect final outcome text, or ERROR: <what failed>>
PROBLEMS: <the exact text ide.context returned, including any duration/timing fields>
EOF
    run_claude_session wt-a "$wt_a" "$prompt" 20
    session_result_text wt-a
}

scenario_worktree_b() {
    origin=$WORKDIR/rust-origin
    [ -d "$origin" ] || fail E_WORKTREE_B_PRECONDITION "run scenario worktree-a first"
    wt_b=$WORKDIR/rust-wt-b
    rm -rf "$wt_b"
    ( cd "$origin" && /usr/bin/git worktree add -q -b wt-b "$wt_b" ) || fail E_WORKTREE_B
    prompt=$WORKDIR/prompt-wt-b.txt
    cat >"$prompt" <<'EOF'
Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with the
same detail_ref. Then run Bash: sleep 4. Then call ide.context with {"kind":"problems"} and
note how long the rust check took (duration_ms, or similar) if reported.
Finally, reply with exactly these two lines and nothing else:
START: <ide.start/ide.inspect final outcome text, or ERROR: <what failed>>
PROBLEMS: <the exact text ide.context returned, including any duration/timing fields>
EOF
    run_claude_session wt-b "$wt_b" "$prompt" 20
    session_result_text wt-b
}

# ---------------------------------------------------------------------------
# Scenario 4: idle shutdown, 30 s after both worktree sessions exit (no real Claude run).
# ---------------------------------------------------------------------------
scenario_idle() {
    origin=$WORKDIR/rust-origin
    [ -d "$origin" ] || fail E_IDLE_PRECONDITION "run scenario worktree-a/worktree-b first"
    runtime=$(rendezvous_runtime_dir "$origin")
    note idle-wait-start "$runtime"
    waited=0
    while [ -d "$runtime" ] && [ "$waited" -lt 60 ]; do
        sleep 2
        waited=$((waited + 2))
    done
    if [ -d "$runtime" ]; then
        note idle-wait-timeout "$runtime still present after ${waited}s"
        echo "FAIL: $runtime still present after ${waited}s"
    else
        note idle-wait-removed "${waited}s"
        echo "PASS: $runtime removed after ${waited}s (idle_timeout_s=30)"
    fi
}

# ---------------------------------------------------------------------------
# Scenario 5: confinement — a worktree outside allowed_roots.
# ---------------------------------------------------------------------------
scenario_confinement() {
    outside=/private/tmp/aiv3-acc-outside-$$
    copy_fixture_repo "$FIXTURES/rust-workspace" "$outside"
    prompt=$WORKDIR/prompt-outside.txt
    cat >"$prompt" <<'EOF'
Call the ide.start tool with no extra arguments. If its reply is pending, call ide.inspect with the
same detail_ref. Then run Bash: sleep 3. Then call ide.context with {"kind":"problems"}.
Finally, reply with exactly these two lines and nothing else:
START: <ide.start/ide.inspect final outcome text, or ERROR: <what failed>>
PROBLEMS: <the exact text ide.context returned>
EOF
    run_claude_session outside "$outside" "$prompt" 20
    session_result_text outside
    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------
case "${1:-}" in
    rust) scenario_rust ;;
    python-venv) scenario_python_venv ;;
    python-noenv) scenario_python_noenv ;;
    worktree-a) scenario_worktree_a ;;
    worktree-b) scenario_worktree_b ;;
    idle) scenario_idle ;;
    confinement) scenario_confinement ;;
    all)
        scenario_rust
        scenario_python_venv
        scenario_python_noenv
        scenario_worktree_a
        scenario_worktree_b
        scenario_idle
        scenario_confinement
        ;;
    *)
        echo "usage: $0 <rust|python-venv|python-noenv|worktree-a|worktree-b|idle|confinement|all>" >&2
        exit 2
        ;;
esac
