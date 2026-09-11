# What tether ships, and the one gate everything passes through on the way out.
#
# Sourced, never run. Three scripts read it: build-clients.sh stages into the
# table's names, ship-binaries.sh ships them, and build-macos.sh uses the same
# staging and the same gate for its fallback path.
#
# CAUTION: this sits at the service root and the container definition sits in
# `container/`, because the repository root ignores any directory called
# `build/`. A build tree that is not committed is a build nobody else can run,
# and nothing would have said so.
#
# # Why the names live here
#
# The name the launcher asks for and the name the build writes are one string.
# Two copies of it in two files is a 404 served to somebody who is already on
# the phone asking for help, so there is one copy and every script reads it.
#
# CAUTION: this file is sourced by a script that runs on a Mac, where bash is
# 3.2 and `stat -c` does not exist. Keep to portable shell.

# The service directory, whatever the caller's working directory is.
TETHER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Where a built artifact waits to be shipped. Gitignored, never DVC-pinned: a
# pinned binary is checked out read-only, which costs it the executable bit and
# turns a working tool into a silent fallback. Share the compilation, never the
# artifact.
TETHER_DIST="$TETHER_DIR/dist"

# The same bytes as `consent::BYPASS_MARKER` in tether-owner. Three files spell
# this string and they must agree: the source that defines it, the Rust test
# that checks both directions, and this gate.
TETHER_BYPASS_MARKER="tether-consent-bypass-compiled-into-this-build"

# A client an owner downloads: the name it is served under, the target that
# produces it, the binary cargo writes, and the file format it must be.
#
# Linux names carry the kernel's word for the architecture and macOS names carry
# Apple's, because the launcher reads `uname -m` on each and passing each
# machine's own spelling through means the name asked for and the name written
# are one string.
#
# There is no 32-bit target and no ARM64 Windows target. Windows on ARM runs the
# x86-64 binary transparently, and a 32-bit machine gets told by name that there
# is no client for it.
TETHER_CLIENTS="
tether-linux-x86_64:x86_64-unknown-linux-musl:tether:elf
tether-linux-aarch64:aarch64-unknown-linux-musl:tether:elf
tether-macos-x86_64:x86_64-apple-darwin:tether:macho
tether-macos-arm64:aarch64-apple-darwin:tether:macho
tether-windows-x86_64.exe:x86_64-pc-windows-gnu:tether.exe:pe
"

# The relay binary the public host runs. Not a download, and built static for
# the same reason the clients are: the box it runs on is not the box it is
# built on.
TETHER_SERVER="
tether-relay-linux-x86_64:x86_64-unknown-linux-musl:tether-relay:elf
"

# A binary this small is a stub or a truncated transfer, whatever else it is.
TETHER_MIN_BYTES=1000000

tether_names() {
    # Every staged name, clients first.
    printf '%s\n%s\n' "$TETHER_CLIENTS" "$TETHER_SERVER" \
        | grep -v '^[[:space:]]*$' | cut -d: -f1
}

tether_field() {
    # tether_field <name> <1-based field>
    printf '%s\n%s\n' "$TETHER_CLIENTS" "$TETHER_SERVER" \
        | grep "^$1:" | cut -d: -f"$2"
}

tether_is_client() {
    printf '%s\n' "$TETHER_CLIENTS" | grep -q "^$1:"
}

tether_stamp() {
    # The stamp both ends of a session show. A stale deployment is then a fact
    # on the screen rather than behaviour nobody can explain, which is why it is
    # a stamp and not a version number somebody has to remember to bump.
    local desc sha
    desc="$(git -C "$TETHER_DIR" describe --tags --always --dirty)"
    sha="$(git -C "$TETHER_DIR" rev-parse --short=9 HEAD)"
    if [ -n "$(git -C "$TETHER_DIR" status --porcelain -- "$TETHER_DIR")" ]; then
        # Not refused: a deploy of an uncommitted fix is sometimes the point.
        # Said out loud, because the stamp is the only thing that remembers.
        echo "   WARNING: the tether tree is dirty; the stamp says so and nothing else will" >&2
    fi
    echo "$desc ($sha)"
}

tether_magic() {
    # The first four bytes, as hex. `od` rather than `xxd`, which is not on
    # every machine this runs on.
    od -An -tx1 -N4 "$1" 2>/dev/null | tr -d ' \n'
}

tether_gate() {
    # tether_gate <staged name>...
    #
    # The last thing that happens before an artifact reaches the address people
    # are read out. Every check here has been earned: a stub binary that shipped
    # because nothing looked at it, a truncated transfer, a build with the
    # consent bypass compiled in.
    local bad=0 name path format magic size
    for name in "$@"; do
        path="$TETHER_DIST/$name"
        format="$(tether_field "$name" 4)"
        if [ -z "$format" ]; then
            echo "   $name: not an artifact this project ships" >&2
            bad=1
            continue
        fi
        if [ ! -f "$path" ]; then
            echo "   $name: missing from $TETHER_DIST" >&2
            bad=1
            continue
        fi
        magic="$(tether_magic "$path")"
        case "$format:$magic" in
            elf:7f454c46*)   ;;
            macho:cffaedfe*) ;;
            pe:4d5a*)        ;;
            *)
                echo "   $name: not a $format file (starts $magic)" >&2
                bad=1
                continue
            ;;
        esac
        size="$(wc -c < "$path" | tr -d ' ')"
        if [ "$size" -lt "$TETHER_MIN_BYTES" ]; then
            echo "   $name: $size bytes, which is too small to be a build" >&2
            bad=1
            continue
        fi
        if [ ! -x "$path" ]; then
            echo "   $name: not executable" >&2
            bad=1
            continue
        fi
        # Absence of the marker is what "a release build has no way past the
        # prompt" means. The Rust suite makes the same check against a host
        # build. This one covers the binary that actually reaches somebody.
        if tether_is_client "$name" && LC_ALL=C grep -qa "$TETHER_BYPASS_MARKER" "$path"; then
            echo "   $name: REFUSING, this build carries the consent bypass" >&2
            echo "   build with no --features, and never with test-consent-bypass" >&2
            bad=1
            continue
        fi
        echo "   $name  $size bytes, $format, no bypass"
    done
    [ "$bad" -eq 0 ]
}

tether_stage() {
    # tether_stage <target dir> <name>
    #
    # Copies one built binary into the stage under the name it is served as.
    local from="$1" name="$2" triple binary src
    triple="$(tether_field "$name" 2)"
    binary="$(tether_field "$name" 3)"
    src="$from/$triple/release/$binary"
    [ -f "$src" ] || { echo "   $name: nothing built at $src" >&2; return 1; }
    mkdir -p "$TETHER_DIST"
    cp "$src" "$TETHER_DIST/$name"
    # An explicit mode, not `chmod +x`. `cp` writes through an existing file and
    # keeps its mode, so restaging over a read-only checkout leaves a mode
    # nobody asked for.
    chmod 755 "$TETHER_DIST/$name"
}
