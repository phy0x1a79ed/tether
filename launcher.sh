#!/bin/sh
# The owner's first contact with tether, and usually their only one.
#
#   curl -fsSL https://nexus.tony-xy-liu.com/tether | sh -s acre anchor kettle
#
# Served by the relay at the mount root, so the address in that line is the
# whole address there is. It fetches the client for this machine, checks that
# the bytes are the ones this release published, checks that the client is one
# this machine can run, runs it with whatever followed `sh -s`, and stops.
# Nothing is installed: no PATH entry, no login item, no launch agent, no
# service, no cron. What is left behind is this directory: the client, and the
# record of the session written beside it. The directory is named below before
# anything runs, and the record is named again in the prompt before the owner
# answers it.
#
# # Why the directory is named for the client's checksum
#
# An owner runs this line once per session, and every run of a random directory
# left another copy of the same client behind. Naming the directory for the
# sha256 the release baked in means one release leaves one directory: a second
# run finds the client already there, checks it against the same hash, and
# runs it without fetching. A new release has a new hash and a new directory.
#
# # Why this is plain sh
#
# The line read out over the phone says `sh` or `bash`, and the machine at the
# other end decides what either of those is: dash on Debian, ash in Alpine,
# busybox in a container, bash 3.2 on a Mac. Anything past POSIX dies on one of
# them before it has said a word, so this script uses nothing past POSIX.
#
# # Why there is a checksum here now
#
# The client no longer comes from the same host as this script. A release
# publishes it to GitHub, this launcher tries GitHub first, and the relay is
# only the fallback for a machine that cannot reach GitHub. Two hosts means the
# old reasoning — alter one, alter the other — no longer holds, so the sha256 of
# every client is baked into this file when the release is cut, and a download
# whose hash does not match is refused rather than run. The owner is still
# trusting the address they were read; the hash is what lets a second host serve
# the bytes without being able to change them.
#
# # Why the arguments are passed straight through
#
# They are the invite code: the word that names the session, then the words
# that are the secret. Nothing to quote and no punctuation. `"$@"` keeps that
# true no matter how many words the code has, so raising the word count is a
# change on the minting side alone.
set -eu

# --- baked by release.sh (do not edit between the markers) ---
# Empty here means this is the source copy, not a released launcher. The source
# refuses to run: a raw checkout has no checksums, so it could only fetch
# unverified. release.sh fills these in when it cuts a release.
TETHER_REPO="phy0x1a79ed/tether"
TETHER_TAG=""
TETHER_SUMS=""
# --- end baked ---

die() { echo "tether: $*" >&2; exit 1; }

[ -n "$TETHER_TAG" ] || die "this is the launcher source, which ships no checksums. Fetch a released launcher from the relay (curl -fsSL https://nexus.tony-xy-liu.com/tether) or from the repository's releases."

# GitHub is the first source; the relay is the fallback a machine behind an
# allow-list that blocks GitHub still reaches. TETHER_RELAY moves the fallback,
# TETHER_GITHUB_BASE moves the first source (an enterprise mirror of the
# release), and TETHER_NO_GITHUB=1 skips GitHub for a machine that cannot or may
# not reach it.
BASE="${TETHER_RELAY:-https://nexus.tony-xy-liu.com/tether}"
GITHUB_BASE="${TETHER_GITHUB_BASE:-https://github.com/$TETHER_REPO/releases/download}"

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
# name the release writes are one string, not two that have to agree.
[ "$os" = linux ] && [ "$arch" = arm64 ] && arch=aarch64
expected="$os-$arch"
asset="${TETHER_ASSET:-tether-$expected}"

# The hash this release published for the asset being fetched. No entry means
# this launcher never shipped that name, which is a mistake to stop on, not to
# paper over by skipping the check.
want="$(printf '%s\n' "$TETHER_SUMS" | awk -v a="$asset" '$2 == a { print $1; exit }')"
[ -n "$want" ] || die "this release published no client called $asset"

# sha256 of a file, by whatever this machine has. Order is deliberate:
# sha256sum on Linux, shasum on a Mac, openssl where neither is. Each prints the
# hash as its first field.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        return 1
    fi
}

# A directory found under that fixed name is reused only when it is this user's
# alone: on a shared box another user can make /tmp/tether.<hash> first and swap
# the client after it is verified. Anything else falls back to a fresh mktemp.
private_dir() {
    [ -d "$1" ] && [ ! -L "$1" ] || return 1
    # shellcheck disable=SC2046
    set -- $(ls -ld "$1")
    case "$1" in drwx------|drwx------[.@]) ;; *) return 1 ;; esac
    [ "$3" = "$(id -un)" ]
}

dir="${TMPDIR:-/tmp}"
dir="${dir%/}/tether.$(printf '%s' "$want" | cut -c1-16)"
if [ -e "$dir" ] || [ -L "$dir" ]; then
    private_dir "$dir" || dir=""
else
    mkdir -m 700 "$dir" 2>/dev/null || dir=""
fi
[ -n "$dir" ] || dir="$(mktemp -d "${TMPDIR:-/tmp}/tether.XXXXXX")"
bin="$dir/tether"

# One download attempt from one URL into $bin. Returns non-zero when the fetch
# fails, so the caller can try the next source. curl follows redirects because a
# GitHub download lands on a second host; wget does the same by default.
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$bin" 2>/dev/null
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$bin" "$1" 2>/dev/null
    else
        die "this machine has neither curl nor wget, so there is no way to fetch the client"
    fi
}

# GitHub first, then the relay. A download that fails falls through to the next
# source; a download that succeeds but whose hash is wrong is refused here and
# does not fall through, because a wrong hash is tampering or corruption and the
# next source cannot be trusted to fix it.
got=""
if [ -f "$bin" ] && [ ! -L "$bin" ] && [ "$(sha256_of "$bin" || true)" = "$want" ]; then
    got="reusing the verified client already in $dir"
fi
for src in github relay; do
    [ -z "$got" ] || break
    [ "$src" = github ] && [ -n "${TETHER_NO_GITHUB:-}" ] && continue
    case "$src" in
        github) url="$GITHUB_BASE/$TETHER_TAG/$asset" ;;
        relay)  url="$BASE/bin/$asset" ;;
    esac
    fetch "$url" || continue
    [ -s "$bin" ] || continue
    have="$(sha256_of "$bin")" \
        || die "this machine has no sha256 tool (sha256sum, shasum or openssl), so the client cannot be verified"
    if [ "$have" != "$want" ]; then
        echo "tether: $url does not match the checksum this release published." >&2
        echo "tether: expected $want" >&2
        echo "tether: received $have" >&2
        die "the download was altered or corrupted; nothing was run."
    fi
    got="fetched $src from $url"
    break
done
[ -n "$got" ] || die "could not download the client for this machine ($asset) from GitHub or $BASE"
echo "tether: $got" >&2

refuse() {
    echo "tether: this machine is $expected, but the client fetched for $asset is $1." >&2
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
