#!/usr/bin/env bash
# Ship what build-clients.sh staged to the host that carries the sessions.
#
#   ./build-clients.sh               build every artifact
#   ./ship-binaries.sh [host]        ship them        default: sirius
#
# The public host has no Rust toolchain and is not getting one: it is a two-core
# box whose job is to carry sockets. It receives the relay it runs, the two
# launchers, and one client per machine an owner might be sitting at:
#
#   <state>/bin/tether-relay          the relay this host runs
#   <state>/assets/tether             the launcher, piped into a shell
#   <state>/assets/tether.ps1         the launcher, for Windows
#   <state>/assets/bin/tether-*       a client the owner downloads
#
# CAUTION: the destination is the service's own state directory, outside the
# checkout. A deploy cleans untracked files, and a multi-megabyte binary inside
# the tree would not survive one.
set -euo pipefail
HOST=${1:-sirius}
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/artifacts.sh"
REMOTE="/var/lib/awm/state/services/tether"
PUBLIC="https://nexus.tony-xy-liu.com/tether"

step() { echo; echo "== $*"; }
remote() { ssh "$HOST" "$@"; }
# sudo runs one command rather than a shell, so `||`, redirection and globbing
# in an argument string would belong to the calling user. Feed a script instead.
as_awm() { ssh "$HOST" "sudo -n -u awm bash -s" <<<"$*"; }
push() {
    # Written beside the live name and moved into place, so a restart or a
    # download that lands mid-transfer gets one whole file, never half of one.
    local src="$1" dst="$2"
    rsync -a --rsync-path='sudo -n -u awm rsync' "$src" "$HOST:$dst.incoming"
    as_awm "mv $dst.incoming $dst"
    as_awm "chmod 755 $dst"
}

step "what is staged"
[ -f "$TETHER_DIST/BUILD_KIND" ] \
    || { echo "nothing staged: run ./build-clients.sh first" >&2; exit 1; }
KIND="$(cat "$TETHER_DIST/BUILD_KIND")"
BUILD="$(cat "$TETHER_DIST/BUILD_STAMP")"
echo "   $BUILD  ($KIND)"
# Only a full container build ships. A host build runs on this box alone, and a
# partial one is one fresh artifact beside a set of stale ones — which is how a
# rebuild that looked like it succeeded ships last week's binary.
[ "$KIND" = cross ] \
    || { echo "refusing to ship a '$KIND' build; run ./build-clients.sh with no arguments" >&2; exit 1; }

step "the gate"
# shellcheck disable=SC2046
tether_gate $(tether_names) || exit 1

step "the remote directories"
as_awm "mkdir -p $REMOTE/bin $REMOTE/assets/bin" \
    || { echo "refusing: cannot write $HOST:$REMOTE — has provision.sh run there?" >&2; exit 1; }

step "ship"
push "$TETHER_DIST/tether-relay-linux-x86_64" "$REMOTE/bin/tether-relay"
push "$HERE/launcher.sh"  "$REMOTE/assets/tether"
push "$HERE/launcher.ps1" "$REMOTE/assets/tether.ps1"
for name in $(printf '%s\n' "$TETHER_CLIENTS" | grep -v '^[[:space:]]*$' | cut -d: -f1); do
    push "$TETHER_DIST/$name" "$REMOTE/assets/bin/$name"
    echo "   $name"
done

step "restart"
# The relay holds every live session in memory, so this ends them. That is the
# design and not a cost to work around: a relay that could hand a session across
# a restart would be a relay that had persisted something.
remote 'sudo -n systemctl restart awm'

step "verify"
# What an owner would get, fetched the way an owner would fetch it. Asking the
# box would only prove the file is on disk; this proves the whole path — nginx,
# the edge mount, the relay's asset directory — answers.
sleep 10
LAUNCHER="$(curl -fsS "$PUBLIC" | head -1)"
[ "$LAUNCHER" = "#!/usr/bin/env bash" ] \
    || { echo "the launcher is not being served: got ${LAUNCHER:-nothing}" >&2; exit 1; }
echo "   the shell launcher"
curl -fsS "$PUBLIC/win" | grep -q 'tether' \
    || { echo "the PowerShell launcher is not being served" >&2; exit 1; }
echo "   the PowerShell launcher"
for name in $(printf '%s\n' "$TETHER_CLIENTS" | grep -v '^[[:space:]]*$' | cut -d: -f1); do
    SIZE="$(curl -fsS -o /dev/null -w '%{size_download}' "$PUBLIC/bin/$name")"
    [ "$SIZE" -gt "$TETHER_MIN_BYTES" ] \
        || { echo "$name downloads as $SIZE bytes" >&2; exit 1; }
    echo "   $name  $SIZE bytes"
done
remote "AWM_WORKSPACE=/opt/awm awm tether status" | python3 -c '
import json, sys
d = json.load(sys.stdin)
c = d.get("child", {})
print("   role=%s built=%s running=%s error=%s"
      % (c.get("role"), c.get("built"), c.get("running"), c.get("error")))
sys.exit(0 if c.get("built") and c.get("running") else 1)'

echo
echo "shipped $BUILD to $HOST"
