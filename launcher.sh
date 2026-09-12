#!/usr/bin/env bash
# The owner's first contact with tether, and usually their only one.
#
#   curl -fsSL https://nexus.tony-xy-liu.com/tether | bash -s 7 anchor kettle
#
# Served by the relay at the mount root, so the address in that line is the
# whole address there is. It fetches the client for this machine, runs it with
# whatever followed `bash -s`, and stops. Nothing is installed: no PATH entry,
# no login item, no launch agent, no service, no cron. What is left behind is
# this directory: the client, and the record of the session written beside it.
# The directory is named below before anything runs, and the record is named
# again in the prompt before the owner answers it.
#
# # Why there is no checksum here
#
# The binary and this script are served by the same host over the same TLS
# connection, so anybody able to alter one can alter the other — and would
# simply write a matching checksum. A hash here would look like a defence and
# be none. What the owner is actually trusting is the address they were read,
# which is why that address is shown again by the client before it asks them
# anything.
#
# # Why the arguments are passed straight through
#
# They are the invite code: a slot and some plain words, nothing to quote and
# no punctuation. `"$@"` keeps that true no matter how many words the code has,
# so raising the word count is a change on the minting side alone.
set -euo pipefail

BASE="${TETHER_RELAY:-https://nexus.tony-xy-liu.com/tether}"

die() { echo "tether: $*" >&2; exit 1; }

case "$(uname -s)" in
    Linux)  os=linux ;;
    Darwin) os=macos ;;
    *) die "this machine runs $(uname -s), which tether has no client for" ;;
esac
case "$(uname -m)" in
    x86_64|amd64)  arch=x86_64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) die "this machine is $(uname -m), which tether has no client for" ;;
esac
# Linux builds are named for the kernel's own word for the architecture; macOS
# builds for Apple's. Keeping each side's spelling means the name here and the
# name the shipping script writes are one string, not two that have to agree.
[ "$os" = linux ] && [ "$arch" = arm64 ] && arch=aarch64
asset="tether-$os-$arch"

dir="$(mktemp -d "${TMPDIR:-/tmp}/tether.XXXXXX")"
bin="$dir/tether"

if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$BASE/bin/$asset" -o "$bin" \
        || die "could not download the client for this machine ($asset) from $BASE"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$bin" "$BASE/bin/$asset" \
        || die "could not download the client for this machine ($asset) from $BASE"
else
    die "this machine has neither curl nor wget, so there is no way to fetch the client"
fi
[ -s "$bin" ] || die "the download was empty; the relay may not be serving a client for $asset"
chmod +x "$bin"

# A file a browser or a download tool fetched is quarantined on macOS, and a
# quarantined binary run from a terminal is refused with a dialog the owner
# cannot answer over the phone. Clearing our own download is the same decision
# the owner would make in that dialog, made where they can see it.
if [ "$os" = macos ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
fi

echo "tether: running $bin" >&2
echo "tether: delete $dir when you are done — the client and its log are in it." >&2

# `exec`, so the client inherits this terminal directly. The consent prompt
# reads the controlling terminal rather than standard input precisely because
# this script arrived through a pipe, and a wrapper process in between would be
# one more thing able to sit between the owner and the question they are being
# asked.
exec "$bin" "$@"
