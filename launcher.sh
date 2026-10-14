#!/bin/sh
# The owner's first contact with tether, and usually their only one.
#
#   curl -fsSL https://nexus.tony-xy-liu.com/tether | sh -s acre anchor kettle
#
# Served by the relay at the mount root, so the address in that line is the
# whole address there is. It fetches the client for this machine, checks that
# the client is one this machine can run, runs it with whatever followed
# `sh -s`, and stops. Nothing is installed: no PATH entry, no login item, no
# launch agent, no service, no cron. What is left behind is this directory: the
# client, and the record of the session written beside it. The directory is
# named below before anything runs, and the record is named again in the prompt
# before the owner answers it.
#
# # Why this is plain sh
#
# The line read out over the phone says `sh` or `bash`, and the machine at the
# other end decides what either of those is: dash on Debian, ash in Alpine,
# busybox in a container, bash 3.2 on a Mac. Anything past POSIX dies on one of
# them before it has said a word, so this script uses nothing past POSIX.
#
# # Why there is no checksum here
#
# The binary and this script are served by the same host over the same TLS
# connection, so anybody able to alter one can alter the other — and would
# simply write a matching checksum. A hash here would look like a defence and
# be none. What the owner is actually trusting is the address they were read,
# which is why that address is shown again by the client before it asks them
# anything. The checks below guard against a wrong pick, not against tampering.
#
# # Why the arguments are passed straight through
#
# They are the invite code: the word that names the session, then the words
# that are the secret. Nothing to quote and no punctuation. `"$@"` keeps that
# true no matter how many words the code has, so raising the word count is a
# change on the minting side alone.
set -eu

BASE="${TETHER_RELAY:-https://nexus.tony-xy-liu.com/tether}"

die() { echo "tether: $*" >&2; exit 1; }

kernel="$(uname -s)"
machine="$(uname -m)"
case "$kernel" in
    Linux)  os=linux ;;
    Darwin) os=macos ;;
    *) die "this machine runs $kernel, which tether has no client for" ;;
esac

# A shell running under Rosetta reports x86_64, and so does everything it
# starts. The hardware answer is one sysctl away, and the native client is the
# one that should run.
if [ "$os" = macos ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
    machine=arm64
fi

# Every spelling a kernel uses for the two architectures there are clients
# for. armv8l is a 64-bit ARM kernel running a 32-bit shell, and a 64-bit
# static binary still runs there. aarch64_be is big-endian, which no client is.
case "$machine" in
    x86_64|amd64|x64)               arch=x86_64 ;;
    aarch64|arm64|arm64e|armv8l)    arch=arm64 ;;
    *) die "this machine is $machine, which tether has no client for" ;;
esac

# Linux builds are named for the kernel's own word for the architecture; macOS
# builds for Apple's. Keeping each side's spelling means the name here and the
# name the shipping script writes are one string, not two that have to agree.
[ "$os" = linux ] && [ "$arch" = arm64 ] && arch=aarch64
expected="$os-$arch"
asset="${TETHER_ASSET:-tether-$expected}"

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

refuse() {
    echo "tether: this machine is $expected, but $BASE/bin/$asset is $1." >&2
    [ -n "${TETHER_ASSET:-}" ] && echo "tether: TETHER_ASSET=$TETHER_ASSET chose that file." >&2
    echo "tether: nothing was run. Tell the person who sent you the code what this said." >&2
    exit 1
}

# What the file is, read off its header: the format, then the CPU field. `dd`
# and `od` because they read bytes the same way under busybox, BSD and GNU.
header="$(dd if="$bin" bs=20 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')"
case "$header" in
    7f454c46????????????????????????????3e00*) found=linux-x86_64 ;;
    7f454c46????????????????????????????b700*) found=linux-aarch64 ;;
    cffaedfe07000001*)                         found=macos-x86_64 ;;
    cffaedfe0c000001*)                         found=macos-arm64 ;;
    7f454c46*)                                 found="a Linux client for another CPU" ;;
    cffaedfe*)                                 found="a macOS client for another CPU" ;;
    4d5a*)                                     found="a Windows client" ;;
    3c*)                                       found="a web page, not a client" ;;
    *)                                         found="not a program this launcher knows (it starts $header)" ;;
esac
[ "$found" = "$expected" ] || refuse "$found"

chmod +x "$bin"

# A file a browser or a download tool fetched is quarantined on macOS, and a
# quarantined binary run from a terminal is refused with a dialog the owner
# cannot answer over the phone. Clearing our own download is the same decision
# the owner would make in that dialog, made where they can see it.
if [ "$os" = macos ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
fi

# The header says what the file claims to be. Running it says whether this
# machine agrees, and the client names the platform it was built for.
status=0
version="$("$bin" --version 2>&1)" || status=$?
[ "$status" -eq 0 ] \
    || refuse "a client that would not start here (exit $status): ${version:-no output}"
case "$arch" in arm64) built=aarch64 ;; *) built=$arch ;; esac
case "$version" in
    *" for $os-$built") ;;
    *" for "*) refuse "a client that says it is for ${version##* for }" ;;
esac

echo "tether: running $bin" >&2
echo "tether: delete $dir when you are done — the client and its log are in it." >&2

# `exec`, so the client inherits this terminal directly. The consent prompt
# reads the controlling terminal rather than standard input precisely because
# this script arrived through a pipe, and a wrapper process in between would be
# one more thing able to sit between the owner and the question they are being
# asked.
exec "$bin" "$@"
