#!/bin/bash
# Probe P4 (v0.3): Python project-wide diagnostics with the pinned Pyright CLI.
# Evidence-only harness: raw JSON, stderr and timings land in out/ next to this
# script; a condensed log lands in out/summary.txt. Nothing outside out/ is written.
#
# Toolchain (accepted product route, see docs/macos-acceptance.md):
#   AGENT_IDE_NODE      pinned node (default: nvm v24.4.0)
#   AGENT_IDE_PYRIGHT   pinned pyright-langserver; the pyright CLI sits next to it
#   TRAIDER_LAB         read-only realistic subject (default: ../traider-lab)
#   NETWORK_PROBE_URL   URL the closed-network check tries to reach (default: pypi)
set -u

PROBE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE="$PROBE_DIR/fixture"
OUT="$PROBE_DIR/out"
mkdir -p "$OUT"
: > "$OUT/summary.txt"

NODE_BIN="${AGENT_IDE_NODE:-/Users/pluto/.nvm/versions/node/v24.4.0/bin/node}"
PYRIGHT_LANGSERVER="${AGENT_IDE_PYRIGHT:-/opt/homebrew/bin/pyright-langserver}"
PYRIGHT="$(dirname "$PYRIGHT_LANGSERVER")/pyright"
export PATH="$(dirname "$NODE_BIN"):$PATH"

TRAIDER_LAB="${TRAIDER_LAB:-/Users/pluto/projects/traider-lab}"
NETWORK_PROBE_URL="${NETWORK_PROBE_URL:-https://pypi.org}"

emit() { tee -a "$OUT/summary.txt" <<< "$*"; }

counts_of() {
  "$NODE_BIN" - "$1" <<'JS' 2>&1
const fs = require("fs");
let d;
try {
  d = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
} catch {
  console.log("  generalDiagnostics: UNPARSEABLE output");
  process.exit(0);
}
const diags = d.generalDiagnostics ?? [];
const bySeverity = { error: 0, warning: 0, information: 0, hint: 0 };
for (const g of diags) bySeverity[g.severity] = (bySeverity[g.severity] ?? 0) + 1;
const s = d.summary ?? {};
console.log(
  `  generalDiagnostics: error=${bySeverity.error} warning=${bySeverity.warning} information=${bySeverity.information} hint=${bySeverity.hint} total=${diags.length}`
);
console.log(
  `  summary: errors=${s.errorCount} warnings=${s.warningCount} infos=${s.informationCount} filesAnalyzed=${s.filesAnalyzed} timeInSec=${s.timeInSec}`
);
JS
}

# run_timed <label> <pyright args...>: one timed --outputjson run via pinned node.
run_timed() {
  local label="$1"; shift
  local code real rss counts
  /usr/bin/time -l "$NODE_BIN" "$PYRIGHT" --outputjson "$@" \
    >"$OUT/$label.json" 2>"$OUT/$label.time"
  code=$?
  real="$(awk '/real/ {print $1}' "$OUT/$label.time")"
  rss="$(awk '/maximum resident set size/ {print $1}' "$OUT/$label.time")"
  emit "$label: exit=$code wall=${real}s maxRSS=$(( ${rss:-0} / 1048576 ))MB"
  counts="$(counts_of "$OUT/$label.json")"
  emit "$counts"
}

emit "== versions: $("$NODE_BIN" "$PYRIGHT" --version 2>&1 | tail -1) on node $("$NODE_BIN" --version)"
emit "== machine: $(/usr/sbin/sysctl -n machdep.cpu.brand_string) / macOS $(sw_vers -productVersion) / $(($( /usr/sbin/sysctl -n hw.memsize) / 1073741824)) GB RAM"

# Q1 exit code 0 on a clean project.
run_timed clean -p "$FIXTURE/clean"

# Q2 cold + repeat on the fixture; cache marker placed before the first run.
touch "$OUT/cache_marker"
run_timed fixture_cold -p "$FIXTURE"
run_timed fixture_repeat -p "$FIXTURE"

{
  echo "project files created/modified by the runs (expect none):"
  find "$FIXTURE" -type f -newer "$OUT/cache_marker"
  echo "files created/modified under \$HOME/Library/Caches (expect none):"
  find "$HOME/Library/Caches" -type f -newer "$OUT/cache_marker" 2>/dev/null | head -20
  echo "files created/modified under TMPDIR outside this probe (expect none):"
  find "${TMPDIR:-/tmp}" -maxdepth 2 -type f -newer "$OUT/cache_marker" 2>/dev/null \
    | grep -v "$PROBE_DIR" | head -20
} > "$OUT/cache_check.txt"
emit "cache: $(find "$FIXTURE" "$HOME/Library/Caches" -type f -newer "$OUT/cache_marker" 2>/dev/null | wc -l | tr -d ' ') new project/cache files; details in out/cache_check.txt"

# Q2 run after a one-line edit (scratch copy keeps the committed fixture pristine).
rm -rf "$OUT/fixture_edit"
cp -R "$FIXTURE" "$OUT/fixture_edit"
printf '\ndef broken(value: int) -> int:\n    return value + "type-error"\n' \
  >> "$OUT/fixture_edit/src/pkg/a.py"
run_timed fixture_edit -p "$OUT/fixture_edit"

# Q1 fatal cases: a config that fails to parse and a config with an unknown enum.
"$NODE_BIN" "$PYRIGHT" -p "$FIXTURE/badconfig" >"$OUT/badconfig.out" 2>"$OUT/badconfig.stderr"
code=$?
emit "fatal badconfig (invalid JSON): exit=$code stderr=$(head -1 "$OUT/badconfig.stderr")"
"$NODE_BIN" "$PYRIGHT" -p "$FIXTURE/badenum" >"$OUT/badenum.out" 2>"$OUT/badenum.stderr"
code=$?
emit "fatal badenum (unknown typeCheckingMode): exit=$code stderr=$(head -1 "$OUT/badenum.stderr")"

# Q2 realistic subject, READ-ONLY: output stays in out/, nothing written into it.
if [ -d "$TRAIDER_LAB" ]; then
  run_timed traider_cold -p "$TRAIDER_LAB"
  run_timed traider_repeat -p "$TRAIDER_LAB"
else
  emit "traider: MISSING $TRAIDER_LAB (skipped)"
fi

# Q3 the analysis must never execute project code.
if [ -f "$FIXTURE/sentinel_written.txt" ]; then
  emit "sentinel: FAIL - project code was executed"
else
  emit "sentinel: PASS - sentinel file absent"
fi

# Q4 record network reachability of the harness environment (see evidence doc).
net_err="$(curl -sS -m 3 -o /dev/null "$NETWORK_PROBE_URL" 2>&1)"
code=$?
emit "network: curl exit=$code to $NETWORK_PROBE_URL (${net_err:-no error})"

# Q5 warm LSP route: spawn -> initialize -> first publishDiagnostics, then the
# stale c.py published only after didOpen.
emit "== warm LSP route =="
"$NODE_BIN" "$PROBE_DIR/lsp_warm.mjs" 2>&1 | while IFS= read -r line; do emit "$line"; done

emit "== done; raw artifacts in $OUT"
