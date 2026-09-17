#!/bin/bash
# v0.3 probe: Rust project-wide diagnostics + build-cache inheritance in a new worktree.
# Usage: run.sh <stage>   stage in {fixture, agent-ide, worktree, offline, analyzer}
# Raw cargo JSON / timing logs land in artifacts/ (gitignored); summaries on stdout.
set -u
PROBE_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXTURE="$PROBE_DIR/fixture"
REPO="$(cd "$PROBE_DIR/../../.." && pwd)"
SCRATCH="$PROBE_DIR/scratch"
ART="$PROBE_DIR/artifacts"
RA="${AGENT_IDE_RUST_ANALYZER:-$HOME/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rust-analyzer}"
mkdir -p "$ART" "$SCRATCH"

# Summarize one cargo JSON stream: errors/warnings by top-level level (children ignored),
# build-finished line, fresh vs rebuilt units from compiler-artifact.
summarize() {
  local label="$1"
  jq -r '
    if .reason == "compiler-message" and .message.level then
      "MSG \(.message.level) \(.message.code.code // "-") \(.target.package_name // "-")"
    elif .reason == "compiler-artifact" and .fresh != null then
      (if .fresh then "FRESH \(.package_id)" else "REBUILD \(.package_id)" end)
    elif .reason == "build-finished" then
      "BUILD-FINISHED success=\(.success)"
    else empty end
  ' "$ART/$label.json" > "$ART/$label.summary"
  echo "== $label: errors=$(grep -c '^MSG error' "$ART/$label.summary" || true) \
warnings=$(grep -c '^MSG warning' "$ART/$label.summary" || true) \
fresh=$(grep -c '^FRESH' "$ART/$label.summary" || true) \
rebuild=$(grep -c '^REBUILD' "$ART/$label.summary" || true) \
$(grep '^BUILD-FINISHED' "$ART/$label.summary" || true)"
  awk '/^(FRESH|REBUILD)/ {n=split($2,p,"#"); print "  " $1 " " substr(p[1], length("path+file://")+1)}' "$ART/$label.summary" | sort | uniq | head -20
}

# run <dir> <label> [extra cargo args...]
run() {
  local dir="$1" label="$2"; shift 2
  ( cd "$dir" && /usr/bin/time -l env CARGO_NET_OFFLINE=true \
      cargo check --workspace --all-targets --message-format=json --keep-going "$@" \
      > "$ART/$label.json" 2> "$ART/$label.time+stderr" )
  echo "-- $label wall: $(awk '/real /{print $1}' "$ART/$label.time+stderr")s  peakRSS: $(grep 'maximum resident' "$ART/$label.time+stderr" | awk '{print $1}')"
  summarize "$label"
}

case "${1:?stage required}" in
fixture)
  rm -rf "$FIXTURE/target"
  run "$FIXTURE" fixture-cold
  run "$FIXTURE" fixture-warm --locked
  ;;
edit)
  printf '// probe edit line\n' >> "$FIXTURE/crates/a/src/lib.rs"
  run "$FIXTURE" fixture-edit --locked
  git -C "$FIXTURE" checkout -q crates/a/src/lib.rs
  ;;
agent-ide)
  rm -rf "$REPO/target"
  run "$REPO" agentide-cold
  run "$REPO" agentide-warm
  ;;
worktree)
  # fixture doubles as a local git repo for the worktree experiment; init on demand
  if [ ! -e "$FIXTURE/.git" ]; then
    git -C "$FIXTURE" init -q
    git -C "$FIXTURE" add -A
    git -C "$FIXTURE" -c commit.gpgsign=false -c user.email=probe@local -c user.name=probe commit -qm "fixture"
  fi
  WT2="$SCRATCH/fixture-wt2"
  rm -rf "$WT2"
  git -C "$FIXTURE" worktree add -q "$WT2" HEAD
  rm -rf "$WT2/target"
  run "$WT2" wt2-cold-empty-target
  /usr/bin/time -p cp -c -R "$FIXTURE/target" "$WT2/target" 2> "$ART/cp-clone.time"
  echo "-- cp -c -R: $(cat "$ART/cp-clone.time" | tr '\n' ' ')"
  echo "-- du source: $(du -sk "$FIXTURE/target" | awk '{print $1}') KiB"
  run "$WT2" wt2-cloned-target
  mv "$FIXTURE" "$FIXTURE.moved-away"
  run "$WT2" wt2-cloned-source-moved
  mv "$FIXTURE.moved-away" "$FIXTURE"
  git -C "$FIXTURE" worktree remove --force "$WT2" 2>/dev/null || true
  df -k "$PROBE_DIR" | tail -1
  ;;
offline)
  NET="$SCRATCH/net-fixture"
  rm -rf "$NET"; mkdir -p "$NET/src"
  cat > "$NET/Cargo.toml" <<'EOF'
[package]
name = "net-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
zzz-v03-probe-unfetched = "1.0"
EOF
  echo 'fn main() {}' > "$NET/src/main.rs"
  ( cd "$NET" && /usr/bin/time -p env CARGO_NET_OFFLINE=true cargo check --workspace --all-targets --message-format=json \
      > "$ART/offline.json" 2> "$ART/offline.time+stderr" )
  echo "-- offline wall: $(awk '/real /{print $1}' "$ART/offline.time+stderr")s"
  grep -m2 -E '^error' "$ART/offline.time+stderr" || true
  ;;
analyzer)
  for d in "$HOME/Library/Caches" "$HOME/.cache"; do
    find "$d" -maxdepth 1 -iname '*rust*' 2>/dev/null >> "$ART/ra-cache-before.txt"
  done
  ( cd "$REPO" && /usr/bin/time -l env CARGO_NET_OFFLINE=true "$RA" analysis-stats . \
      > "$ART/ra-analysis-stats.txt" 2> "$ART/ra.time+stderr" )
  echo "-- rust-analyzer wall: $(awk '/real /{print $1}' "$ART/ra.time+stderr")s  peakRSS: $(grep 'maximum resident' "$ART/ra.time+stderr" | awk '{print $1}')"
  for d in "$HOME/Library/Caches" "$HOME/.cache"; do
    find "$d" -maxdepth 1 -iname '*rust*' 2>/dev/null >> "$ART/ra-cache-after.txt"
  done
  diff "$ART/ra-cache-before.txt" "$ART/ra-cache-after.txt" && echo "-- no new rust-* cache dirs"
  ;;
agentide-edit)
  printf '// probe edit line\n' >> "$REPO/src/lib.rs"
  run "$REPO" agentide-edit
  git -C "$REPO" checkout -q src/lib.rs
  ;;
agent-ide-wt2)
  WT2="$SCRATCH/agentide-wt2"
  git -C "$REPO" worktree remove --force "$WT2" 2>/dev/null || true
  git -C "$REPO" worktree add -q "$WT2" HEAD
  rm -rf "$WT2/target"
  DF0=$(df -k /System/Volumes/Data | awk 'NR==2{print $4}')
  /usr/bin/time -p cp -c -R "$REPO/target" "$WT2/target" 2> "$ART/cp-clone-agentide.time"
  DF1=$(df -k /System/Volumes/Data | awk 'NR==2{print $4}')
  echo "-- cp -c -R target: $(awk '/real /{print $2"s"}' "$ART/cp-clone-agentide.time")  du source=$(du -sk "$REPO/target" | awk '{print $1}')KiB  df-delta=$((DF0-DF1))KiB"
  run "$WT2" wt2-agentide-cloned-target
  git -C "$REPO" worktree remove --force "$WT2"
  rm -rf "$WT2"
  ;;
esac
