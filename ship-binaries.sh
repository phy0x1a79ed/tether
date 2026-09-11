#!/usr/bin/env bash
# Build this node's tether binaries and ship them to a node that cannot build.
#
#   ./ship-binaries.sh [host]        default: sirius
#
# The public host has no Rust toolchain and is not getting one: it is a two-core
# box whose job is to carry sockets. It receives three artifacts, and the same
# trip serves both of the relay's roles — the binary it runs, and the downloads
# it hands to owners:
#
#   <state>/bin/tether-relay          the relay this host runs
#   <state>/assets/tether             the launcher, piped into a shell
#   <state>/assets/bin/tether-linux-* a client the owner downloads
#
# The macOS client is not built here and cannot be; it is shipped separately
# into the same assets directory. Its absence is visible rather than silent: a
# Mac owner's launcher asks for a name the relay does not have and says so.
#
# CAUTION: the destination is the service's own state directory, outside the
# checkout. A deploy cleans untracked files, and a multi-megabyte binary inside
# the tree would not survive one.
set -euo pipefail
HOST=${1:-sirius}
HERE="$(cd "$(dirname "$0")" && pwd)"
MANIFEST="$HERE/rust/Cargo.toml"
TARGET="$HERE/rust/target/release"
REMOTE="/var/lib/awm/state/services/tether"

step() { echo; echo "== $*"; }
remote() { ssh "$HOST" "$@"; }
# sudo runs one command rather than a shell, so `||`, redirection and globbing
# in an argument string would belong to the calling user. Feed a script instead.
as_awm() { ssh "$HOST" "sudo -n -u awm bash -s" <<<"$*"; }

step "the build stamp"
# Both binaries carry this through `option_env!`, and both ends of a session
# show it. A stale deployment is then a fact on the screen rather than
# behaviour nobody can explain — which is the whole reason it is a stamp and
# not a version number somebody has to remember to bump.
DESC="$(git -C "$HERE" describe --tags --always --dirty)"
SHA="$(git -C "$HERE" rev-parse --short=9 HEAD)"
BUILD="$DESC ($SHA)"
if [ -n "$(git -C "$HERE" status --porcelain -- "$HERE")" ]; then
    # Not refused, because a deploy of an uncommitted fix is sometimes the
    # point. Said out loud, because the stamp is the only thing that will
    # remember afterwards.
    echo "   WARNING: the tether tree is dirty; the stamp says so and nothing else will" >&2
fi
echo "   $BUILD"

step "build"
command -v cargo >/dev/null 2>&1 || { echo "no cargo on this box" >&2; exit 1; }
TETHER_BUILD="$BUILD" cargo build --release --manifest-path "$MANIFEST"
for b in tether-relay tether; do
    [ -x "$TARGET/$b" ] || { echo "the build produced no $TARGET/$b" >&2; exit 1; }
done
# The client the owner downloads is named for the machine that will run it, and
# the launcher builds that same name from `uname`. The two spellings are one
# string in two files; changing either alone serves a 404 to somebody asking
# for help.
CLIENT="tether-linux-$(uname -m)"
echo "   relay $(stat -c %s "$TARGET/tether-relay") bytes, client $(stat -c %s "$TARGET/tether") bytes as $CLIENT"

step "the remote directories"
as_awm "mkdir -p $REMOTE/bin $REMOTE/assets/bin" \
    || { echo "refusing: cannot write $HOST:$REMOTE — has provision.sh run there?" >&2; exit 1; }

step "ship"
# Written beside the live name and moved into place, so a restart that lands
# mid-transfer finds either the old binary or the new one, never half of one.
rsync -a --rsync-path='sudo -n -u awm rsync' \
    "$TARGET/tether-relay" "$HOST:$REMOTE/bin/tether-relay.incoming"
rsync -a --rsync-path='sudo -n -u awm rsync' \
    "$TARGET/tether" "$HOST:$REMOTE/assets/bin/$CLIENT.incoming"
rsync -a --rsync-path='sudo -n -u awm rsync' \
    "$HERE/launcher.sh" "$HOST:$REMOTE/assets/tether.incoming"
as_awm "mv $REMOTE/bin/tether-relay.incoming $REMOTE/bin/tether-relay"
as_awm "mv $REMOTE/assets/bin/$CLIENT.incoming $REMOTE/assets/bin/$CLIENT"
as_awm "mv $REMOTE/assets/tether.incoming $REMOTE/assets/tether"
as_awm "chmod 755 $REMOTE/bin/tether-relay $REMOTE/assets/bin/$CLIENT $REMOTE/assets/tether"

step "restart"
# The relay holds every live session in memory, so this ends them. That is the
# design and not a cost to work around: a relay that could hand a session
# across a restart would be a relay that had persisted something.
remote 'sudo -n systemctl restart awm'

step "verify"
sleep 10
# What an owner would get, fetched the way an owner would fetch it. Asking the
# box would only prove the file is on disk; this proves the whole path — nginx,
# the edge mount, the relay's asset directory — answers.
LAUNCHER="$(curl -fsS "https://nexus.tony-xy-liu.com/tether" | head -1)"
[ "$LAUNCHER" = "#!/usr/bin/env bash" ] \
    || { echo "the launcher is not being served: got ${LAUNCHER:-nothing}" >&2; exit 1; }
SIZE="$(curl -fsS -o /dev/null -w '%{size_download}' "https://nexus.tony-xy-liu.com/tether/bin/$CLIENT")"
[ "$SIZE" -gt 1000000 ] || { echo "the client download is $SIZE bytes" >&2; exit 1; }
remote "AWM_WORKSPACE=/opt/awm awm tether status" | python3 -c '
import json, sys
d = json.load(sys.stdin)
c = d.get("child", {})
print("   role=%s built=%s running=%s error=%s"
      % (c.get("role"), c.get("built"), c.get("running"), c.get("error")))
sys.exit(0 if c.get("built") and c.get("running") else 1)'
echo "shipped $BUILD to $HOST"
