#!/usr/bin/env bash
# Build every client an owner can download, in one container, for every machine.
#
#   ./build-clients.sh --image          build the container that builds them (once, and on a base bump)
#   ./build-clients.sh                  build every target and stage it
#   ./build-clients.sh --target <triple>  one target, for the inner loop
#   ./build-clients.sh --host           this machine's target, no container
#   ./build-clients.sh --gate           check what is staged
#   ./build-clients.sh --clean          empty the stage
#
# # Why a container
#
# The owner downloads this binary onto a machine nobody chose in advance. A
# client built against the build box's C library runs only on machines no older
# than the build box, which is the opposite of what a download onto somebody
# else's machine has to be. The first machine this was ever pointed at refused
# to start it and named a glibc version the owner had no way to act on.
#
# So every client is built against a toolchain that is pinned rather than
# whatever this box happens to have: musl for Linux, which links statically and
# needs nothing from the machine it lands on, osxcross for the two Apple
# targets, and mingw-w64 for Windows. `container/Dockerfile` carries them.
#
# The relay is built the same way for the same reason. It runs on a box we own,
# but not the box it is built on.
#
# CAUTION: a `--host` build is dynamically linked against this machine. It is
# right for testing here and wrong to ship, and nothing about the file says so,
# so it writes `local` into the stage's BUILD_KIND and ship-binaries.sh refuses
# that.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/artifacts.sh"

# Pinned. `container/Dockerfile` layers onto this and must never replace it: the
# base is the only thing that can link a Darwin target here.
BASE_IMAGE="joseluisq/rust-linux-darwin-builder:2.0.0-beta.1"
IMAGE="tether-build:${BASE_IMAGE##*:}"

# Cargo's build directory, deliberately outside the worktree and shared by every
# scope on this machine. Registry downloads and the cross targets are then built
# once rather than once per worktree. Cargo locks the directory, so two scopes
# building at once block rather than corrupt.
#
# Split in two because the container runs as root and leaves root-owned files
# behind. A host build sharing one root fails on cargo's own bookkeeping at the
# top of it. They are different toolchains against different targets anyway.
CACHE="${TETHER_BUILD_CACHE:-$HOME/.cache/tether}"
CROSS_TARGET_DIR="$CACHE/cross"
HOST_TARGET_DIR="$CACHE/host"
REGISTRY_DIR="$CACHE/registry"

step() { echo; echo "== $*"; }

in_container() {
    # The target directory is mounted at its own host path, not at a
    # container-local one: cargo records absolute paths in its fingerprints, so
    # the directory has to be called the same thing on both sides or every build
    # invalidates the last one's work.
    #
    # The registry is mounted too. Without it every target in the loop
    # re-downloads the whole dependency graph.
    #
    # TETHER_DOCKER_DNS is an escape hatch. crates.io fronts on a CDN whose IPv6
    # address stalls in a bridge network with no IPv6 route, and the symptom is a
    # build that hangs rather than one that fails.
    docker run --rm \
        ${TETHER_DOCKER_DNS:+--dns "$TETHER_DOCKER_DNS"} \
        --mount type=bind,source="$HERE/rust",target=/root/src \
        --mount type=bind,source="$CROSS_TARGET_DIR",target="$CROSS_TARGET_DIR" \
        --mount type=bind,source="$REGISTRY_DIR",target=/root/.cargo/registry \
        --env CARGO_TARGET_DIR="$CROSS_TARGET_DIR" \
        --env TETHER_BUILD="${TETHER_BUILD:-}" \
        --env WINEPREFIX=/tmp/wineprefix \
        --env WINEDEBUG=-all \
        --env XDG_RUNTIME_DIR=/tmp \
        ${TETHER_RELAY:+--env TETHER_RELAY="$TETHER_RELAY"} \
        --workdir /root/src \
        "$IMAGE" \
        "$@"
}

triples() {
    printf '%s\n%s\n' "$TETHER_CLIENTS" "$TETHER_SERVER" \
        | grep -v '^[[:space:]]*$' | cut -d: -f2 | awk '!seen[$0]++'
}

# Which packages a target has to build. tether-operator and tether-relay are
# Linux-only by construction — unix sockets, unix signals — so asking for the
# whole workspace fails every non-Linux target before it compiles anything.
packages_for() {
    local triple="$1" bin out=""
    for bin in $(printf '%s\n%s\n' "$TETHER_CLIENTS" "$TETHER_SERVER" \
                    | grep ":$triple:" | cut -d: -f3 | sort -u); do
        case "$bin" in
            tether|tether.exe) out="$out -p tether-owner" ;;
            tether-relay)      out="$out -p tether-relay" ;;
        esac
    done
    echo "$out"
}

names_for() {
    printf '%s\n%s\n' "$TETHER_CLIENTS" "$TETHER_SERVER" \
        | grep ":$1:" | cut -d: -f1
}

build_target() {
    local triple="$1"
    step "$triple"
    # shellcheck disable=SC2046
    in_container cargo build --release --target "$triple" $(packages_for "$triple")
    local name
    for name in $(names_for "$triple"); do
        tether_stage "$CROSS_TARGET_DIR" "$name"
        echo "   staged $name"
    done
}

mkdir -p "$CROSS_TARGET_DIR" "$HOST_TARGET_DIR" "$REGISTRY_DIR"

case "${1:---all}" in
    --image)
        # Pull the base explicitly rather than letting the build resolve it, so
        # a base that has moved is a visible step rather than a silent layer.
        step "the base"
        docker pull "$BASE_IMAGE"
        step "the layer"
        docker build --build-arg "BASE=$BASE_IMAGE" -t "$IMAGE" "$HERE/container"
        echo
        echo "built $IMAGE"
    ;;
    --all)
        docker image inspect "$IMAGE" >/dev/null 2>&1 \
            || { echo "no $IMAGE: run ./build-clients.sh --image first" >&2; exit 1; }
        step "the build stamp"
        TETHER_BUILD="$(tether_stamp)"
        export TETHER_BUILD
        echo "   $TETHER_BUILD"
        for triple in $(triples); do
            build_target "$triple"
        done
        echo "cross" > "$TETHER_DIST/BUILD_KIND"
        echo "$TETHER_BUILD" > "$TETHER_DIST/BUILD_STAMP"
        step "the gate"
        # shellcheck disable=SC2046
        tether_gate $(tether_names) || exit 1
        step "the Windows smoke test"
        # The only check on a Windows build this box can make, and it is worth
        # more than it looks. The second run reaches a port nothing is on, so a
        # refused connection is the *success* case: getting that far means the
        # argument parsing, the invite grammar, the TLS stack and the socket
        # layer all came up under Windows' own runtime. Only a person at a real
        # Windows machine can check the rest.
        if in_container bash -c 'command -v wine >/dev/null 2>&1'; then
            exe="$CROSS_TARGET_DIR/x86_64-pc-windows-gnu/release/tether.exe"
            in_container wine "$exe" --version \
                || { echo "the Windows client did not run under wine" >&2; exit 1; }
            # The output is captured rather than piped, and the failure
            # swallowed: a client that cannot reach a relay exits non-zero, and
            # that non-zero *is* the success being looked for here.
            export TETHER_RELAY="https://127.0.0.1:1/tether"
            dialled="$(in_container wine "$exe" 7 anchor kettle 2>&1 || true)"
            unset TETHER_RELAY
            case "$dialled" in
                *"could not reach the relay"*) echo "   it runs, and it dials" ;;
                *) echo "the Windows client did not get as far as dialling:" >&2
                   echo "$dialled" >&2
                   exit 1 ;;
            esac
        else
            echo "   skipped: no wine in $IMAGE"
        fi
        echo
        echo "staged $TETHER_BUILD to $TETHER_DIST"
    ;;
    --target)
        shift
        [ $# -ge 1 ] || { echo "which target?" >&2; exit 1; }
        TETHER_BUILD="$(tether_stamp)"
        export TETHER_BUILD
        build_target "$1"
        # Partial, and said so. ship-binaries.sh refuses anything but a full
        # container build, which is what stops one fresh slot beside four stale
        # ones from shipping as a set.
        echo "cross-partial" > "$TETHER_DIST/BUILD_KIND"
        echo "$TETHER_BUILD" > "$TETHER_DIST/BUILD_STAMP"
    ;;
    --host)
        # This machine's toolchain, this machine's target, no container. Seconds
        # rather than minutes, and dynamically linked against this box.
        step "the build stamp"
        TETHER_BUILD="$(tether_stamp)"
        export TETHER_BUILD
        echo "   $TETHER_BUILD"
        step "build"
        CARGO_TARGET_DIR="$HOST_TARGET_DIR" \
            cargo build --release --manifest-path "$HERE/rust/Cargo.toml"
        # The stage is cleared first. A host build refreshes one slot, and
        # leaving the others behind from an earlier run is how a stale binary
        # survives a rebuild that looked like it succeeded.
        rm -rf "$TETHER_DIST"
        mkdir -p "$TETHER_DIST"
        # Each machine's own spelling of its architecture, which is what the
        # launcher asks for: the kernel's on Linux, Apple's on a Mac.
        case "$(uname -s)" in
            Linux)  os=linux ;;
            Darwin) os=macos ;;
            *) echo "no client name for $(uname -s)" >&2; exit 1 ;;
        esac
        arch="$(uname -m)"
        cp "$HOST_TARGET_DIR/release/tether" "$TETHER_DIST/tether-$os-$arch"
        chmod 755 "$TETHER_DIST/tether-$os-$arch"
        echo "local" > "$TETHER_DIST/BUILD_KIND"
        echo "$TETHER_BUILD" > "$TETHER_DIST/BUILD_STAMP"
        echo
        echo "staged a HOST-LINKED tether-$os-$arch — fine to test with, refused by ship-binaries.sh"
    ;;
    --gate)
        # shellcheck disable=SC2046
        tether_gate $(tether_names)
    ;;
    --clean)
        rm -rf "$TETHER_DIST"
        echo "removed $TETHER_DIST"
    ;;
    *)
        echo "unknown option: $1" >&2
        exit 1
    ;;
esac
