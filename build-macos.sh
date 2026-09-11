#!/usr/bin/env bash
# Build the macOS client on a Mac and ship it to the relay.
#
#   ./build-macos.sh [host]        default: sirius
#
# This is the one command an owner's Mac needs, and it is separate from
# ship-binaries.sh for a reason that is not going away: producing a macOS
# binary needs Apple's SDK, and a Linux box cross-compiling to Darwin needs
# that same SDK copied onto it. The tool ships rarely enough that building on
# the machine it is for costs less than keeping a cross toolchain honest.
#
# What this box needs first:
#   - a checkout of this repository
#   - a Rust toolchain (https://rustup.rs)
#
# It builds only the owner's client. The relay and the operator daemon are
# Linux-only and ship from a Linux box.
#
# CAUTION: this refuses to ship a binary carrying the consent bypass. That
# check is the last gate before a build reaches the address people are read
# out, and it is cheap enough to run every time.
set -euo pipefail
HOST=${1:-sirius}
HERE="$(cd "$(dirname "$0")" && pwd)"
MANIFEST="$HERE/rust/Cargo.toml"
TARGET="$HERE/rust/target/release"
REMOTE="/var/lib/awm/state/services/tether"
# The same bytes as `consent::BYPASS_MARKER` in tether-owner. Three files
# spell this string and they must agree: the source that defines it, the
# Rust test that checks both directions, and this last gate.
BYPASS_MARKER="tether-consent-bypass-compiled-into-this-build"

step() { echo; echo "== $*"; }
as_awm() { ssh "$HOST" "sudo -n -u awm bash -s" <<<"$*"; }

[ "$(uname -s)" = Darwin ] \
    || { echo "this is $(uname -s); run it on the Mac the client is for" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 \
    || { echo "no cargo on this Mac; install a toolchain from https://rustup.rs" >&2; exit 1; }

step "the build stamp"
# The same stamp ship-binaries.sh writes, so the two ends of a session report
# their builds in one vocabulary and a stale download is a fact on the screen.
DESC="$(git -C "$HERE" describe --tags --always --dirty)"
SHA="$(git -C "$HERE" rev-parse --short=9 HEAD)"
BUILD="$DESC ($SHA)"
if [ -n "$(git -C "$HERE" status --porcelain -- "$HERE")" ]; then
    echo "   WARNING: the tether tree is dirty; the stamp says so and nothing else will" >&2
fi
echo "   $BUILD"

step "build"
TETHER_BUILD="$BUILD" cargo build --release --manifest-path "$MANIFEST" -p tether-owner
[ -x "$TARGET/tether" ] || { echo "the build produced no $TARGET/tether" >&2; exit 1; }

# Apple spells the architectures its own way and the launcher reads `uname -m`
# the same way, so the name written here and the name asked for there are one
# string rather than two that have to agree.
case "$(uname -m)" in
    arm64)  CLIENT=tether-macos-arm64 ;;
    x86_64) CLIENT=tether-macos-x86_64 ;;
    *) echo "this Mac is $(uname -m), which the launcher has no name for" >&2; exit 1 ;;
esac
echo "   $(wc -c < "$TARGET/tether") bytes as $CLIENT"

step "the consent gate"
# Absence of the marker is what "a release build has no way past the prompt"
# means. The Rust suite makes the same check against a Linux build; this one
# covers the binary that actually reaches a Mac, which no test on a build box
# can see.
if LC_ALL=C grep -qa "$BYPASS_MARKER" "$TARGET/tether"; then
    echo "REFUSING: this build carries the consent bypass" >&2
    echo "Run a plain 'cargo build --release', with no --features." >&2
    exit 1
fi
echo "   no bypass in the binary"

step "ship"
as_awm "mkdir -p $REMOTE/assets/bin" \
    || { echo "refusing: cannot write $HOST:$REMOTE — has provision.sh run there?" >&2; exit 1; }
# Written beside the live name and moved into place, so a download that lands
# mid-transfer gets one whole file or the other.
rsync -a --rsync-path='sudo -n -u awm rsync' \
    "$TARGET/tether" "$HOST:$REMOTE/assets/bin/$CLIENT.incoming"
as_awm "mv $REMOTE/assets/bin/$CLIENT.incoming $REMOTE/assets/bin/$CLIENT"
as_awm "chmod 755 $REMOTE/assets/bin/$CLIENT"

step "verify"
# Fetched the way an owner would fetch it, over the public address. Asking the
# box would prove only that a file is on disk.
SIZE="$(curl -fsS -o /dev/null -w '%{size_download}' \
    "https://nexus.tony-xy-liu.com/tether/bin/$CLIENT")"
[ "$SIZE" -gt 1000000 ] || { echo "the client download is $SIZE bytes" >&2; exit 1; }
echo "   $SIZE bytes served at /tether/bin/$CLIENT"

echo
echo "shipped $BUILD as $CLIENT"
echo "a Mac owner's line is unchanged:"
echo "  curl -fsSL https://nexus.tony-xy-liu.com/tether | bash -s <code>"
