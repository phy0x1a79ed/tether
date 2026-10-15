#!/usr/bin/env bash
# Build the macOS client on a Mac and stage it for release.
#
#   ./build-macos.sh
#
# # When to reach for this
#
# Not usually. `./build-clients.sh` cross-compiles both Apple targets in a
# container that carries Apple's SDK, so a Mac client builds on the Linux box
# with every other client, stamped the same and gated the same. This script is
# the fallback for a session where that container cannot produce a Darwin
# binary, and the proof that a Mac alone is still enough.
#
# What this box needs first:
#   - a checkout of this repository
#   - a Rust toolchain (https://rustup.rs)
#
# It builds only the owner's client. The relay and the operator daemon are
# Linux-only and build on a Linux box.
#
# CAUTION: this stages one artifact and leaves the rest of the stage alone, so
# the stage stops being one coherent set. It writes that into BUILD_KIND, and
# release.sh refuses to publish a set marked that way.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/artifacts.sh"
MANIFEST="$HERE/Cargo.toml"
TARGET="$HERE/target/release"

step() { echo; echo "== $*"; }

[ "$(uname -s)" = Darwin ] \
    || { echo "this is $(uname -s); run it on the Mac the client is for" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 \
    || { echo "no cargo on this Mac; install a toolchain from https://rustup.rs" >&2; exit 1; }

step "the build stamp"
# The same stamp build-clients.sh writes, so the two ends of a session report
# their builds in one vocabulary and a stale download is a fact on the screen.
BUILD="$(tether_stamp)"
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

step "stage"
mkdir -p "$TETHER_DIST"
cp "$TARGET/tether" "$TETHER_DIST/$CLIENT"
chmod 755 "$TETHER_DIST/$CLIENT"
echo "mac-only" > "$TETHER_DIST/BUILD_KIND"
echo "$BUILD" > "$TETHER_DIST/BUILD_STAMP"
echo "   $CLIENT"

step "the gate"
# The same gate every other artifact passes, including the check that a released
# build carries no way past the consent prompt.
tether_gate "$CLIENT" || exit 1

echo
echo "staged $BUILD as $CLIENT in $TETHER_DIST"
echo "release.sh picks it up from there; it will refuse this mac-only stage"
echo "until the Linux targets are staged beside it."
