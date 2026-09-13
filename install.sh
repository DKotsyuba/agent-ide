#!/bin/sh
set -eu

repo="${AGENT_IDE_REPOSITORY:-DKotsyuba/agent-ide}"
install_dir="${AGENT_IDE_INSTALL_DIR:-$HOME/.local/bin}"
version="${1:-latest}"

test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
  echo "agent-ide supports macOS arm64 only" >&2
  exit 1
}
command -v gh >/dev/null 2>&1 || {
  echo "gh is required to download the private GitHub Release" >&2
  exit 1
}

if test "$version" = latest; then
  tag="$(gh release view --repo "$repo" --json tagName --jq .tagName)"
else
  case "$version" in
    v*) tag="$version" ;;
    *) tag="v$version" ;;
  esac
fi

asset="agent-ide-${tag}-aarch64-apple-darwin.tar.gz"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-install.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
gh release download "$tag" --repo "$repo" --pattern "$asset" --pattern SHA256SUMS --dir "$tmp_dir"
(cd "$tmp_dir" && shasum -a 256 -c SHA256SUMS)
tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"

mkdir -p "$install_dir"
tmp_binary="$install_dir/.agent-ide.$$"
cp "$tmp_dir/agent-ide" "$tmp_binary"
chmod 755 "$tmp_binary"
mv -f "$tmp_binary" "$install_dir/agent-ide"
echo "installed $tag to $install_dir/agent-ide"
