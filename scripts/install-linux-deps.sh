#!/bin/sh
# Install current Git and bubblewrap for Agent IDE on Ubuntu through APT.
# This adds the Git maintainers' PPA and updates system packages through APT.

set -eu

fail() {
    printf 'install-linux-deps.sh: %s\n' "$1" >&2
    exit 1
}

if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then
    printf '%s\n' \
        'Usage: sudo sh scripts/install-linux-deps.sh' \
        'Adds the Git stable PPA and installs Git and bubblewrap on Ubuntu.'
    exit 0
fi
[ "$#" -eq 0 ] || fail 'no arguments are supported; use --help'
[ "$(id -u)" -eq 0 ] || fail 'run with sudo: sudo sh scripts/install-linux-deps.sh'
[ -r /etc/os-release ] || fail 'cannot identify the Linux distribution'
. /etc/os-release
[ "${ID:-}" = ubuntu ] || fail 'this installer supports Ubuntu only'

if ! command -v add-apt-repository >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
    apt-get -o APT::Update::Error-Mode=any update
    apt-get install -y software-properties-common curl ca-certificates
fi

page=$(curl -fsSL --connect-timeout 15 --max-time 45 https://git-scm.com/install/linux.html) ||
    fail 'cannot read the latest Git version from git-scm.com'
latest=$(printf '%s\n' "$page" | sed -n 's/^[[:space:]]*Latest version:[[:space:]]*\([0-9][0-9.]*\).*/\1/p')
printf '%s\n' "$latest" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    fail 'cannot identify the latest stable Git version'

add-apt-repository -y --no-update ppa:git-core/ppa
apt-get -o APT::Update::Error-Mode=any update
candidate=$(LC_ALL=C apt-cache policy git | sed -n 's/^[[:space:]]*Candidate:[[:space:]]*//p')
[ -n "$candidate" ] && [ "$candidate" != '(none)' ] || fail 'APT has no Git candidate'
candidate_upstream=${candidate#*:}
candidate_upstream=${candidate_upstream%%-*}
dpkg --compare-versions "$candidate_upstream" ge "$latest" ||
    fail "latest Git is $latest, but APT offers $candidate; check package pins or wait for the PPA"

apt-get install -y git bubblewrap
git_version=$(/usr/bin/git --version)
installed=${git_version#git version }
dpkg --compare-versions "$installed" ge "$latest" ||
    fail "installed $git_version is older than $latest; check APT holds or package pins"
/usr/bin/git --no-lazy-fetch --version >/dev/null || fail 'installed Git lacks --no-lazy-fetch'

bwrap_help=$(/usr/bin/bwrap --help)
for option in '--bind-fd FD' '--ro-bind-fd FD' '--disable-userns' '--assert-userns-disabled'; do
    case "$bwrap_help" in
        *"$option"*) ;;
        *) fail "installed bubblewrap lacks $option" ;;
    esac
done

# Test namespaces as the invoking user when started through sudo, rather than as root.
check_bwrap() {
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        sudo -u "$SUDO_USER" -- /usr/bin/bwrap "$@"
    else
        /usr/bin/bwrap "$@"
    fi
}
check_bwrap --unshare-user --unshare-pid --unshare-net --disable-userns \
    --assert-userns-disabled --ro-bind / / -- /usr/bin/true ||
    fail 'bubblewrap is installed, but its namespace isolation could not start'

printf '%s\n' "$git_version" "$(/usr/bin/bwrap --version)"
printf '%s\n' 'If the IDE launcher pins the Git digest, remeasure /usr/bin/git and restart the IDE.'
