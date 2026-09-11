# The one toolchain that builds every client an owner can download.
#
#   ../build-clients.sh --image      builds this
#
# # Why this layers rather than starts from a Rust image
#
# The base carries osxcross, which is the only reason the two Apple targets can
# be linked on a Linux box at all. Replacing it with a plain Rust image and
# adding targets by hand produces a build that succeeds for Linux and fails both
# Darwin targets at the linker. The metasmith engine shipped a release that way
# once: stub binaries, three of four platforms broken, found by a user. Layer
# onto the base. Never reproduce it.
#
# CAUTION: the base tag is pinned, and build-clients.sh derives the local tag
# from it. Bumping one bumps the other, so a base change is visible in
# `docker images` rather than silent.
ARG BASE=joseluisq/rust-linux-darwin-builder:2.0.0-beta.1
FROM ${BASE}

# What tether needs and the base does not carry.
#
#   mingw-w64  links the Windows target. Rust ships the import libraries and no
#              linker, so without this the Windows build fails at the last step
#              with every object file already compiled.
#   nasm       assembles ring's x86-64 code for Windows. ring is the only crate
#              in this workspace that compiles anything but Rust, and Windows is
#              the one target where it hands its assembly to nasm rather than to
#              the C compiler.
#   wine       runs the finished .exe here, which is the only check on a Windows
#              build this box can make. It is optional on purpose. A base whose
#              distribution moved the package leaves the build working and the
#              smoke test skipped, rather than failing the image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends mingw-w64 nasm \
    && { apt-get install -y --no-install-recommends wine64 \
         || apt-get install -y --no-install-recommends wine \
         || echo "no wine in this base: the Windows smoke test will be skipped"; } \
    && rm -rf /var/lib/apt/lists/*

# The compiler pinned here, not the base's. The base tracks its own Rust
# version, and a client built by a different compiler than the one the operator
# daemon was built by is a difference nobody asked for and nobody can see. This
# is the version the workspace is developed against, so the lock file resolves
# the same way in both places.
ARG RUST_VERSION=1.98.1
RUN rustup toolchain install ${RUST_VERSION} --profile minimal \
    && rustup default ${RUST_VERSION} \
    && rustup target add --toolchain ${RUST_VERSION} \
        x86_64-unknown-linux-musl \
        aarch64-unknown-linux-musl \
        x86_64-apple-darwin \
        aarch64-apple-darwin \
        x86_64-pc-windows-gnu

# Named rather than inferred, once per target.
#
# The base's cargo config already names a *linker* per target, which is what
# rustc needs. It names no C compiler, which is what the `cc` crate needs, and
# `cc` falls back to this machine's own `cc` — so ring compiles x86-64 Linux
# objects and hands them to a Darwin linker, and the build gets most of the way
# through before it cannot link. ring is the only crate here that compiles C,
# and these lines are the whole of what it takes.
#
# CAUTION: the musl archiver is binutils, not a musl-prefixed tool. The base
# ships `x86_64-linux-musl-gcc` and no `x86_64-linux-musl-ar`, and naming one
# fails the build inside ring's build script with a message about a missing
# file rather than about a missing toolchain.
ENV CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc \
    AR_x86_64_unknown_linux_musl=x86_64-linux-gnu-ar \
    CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
    AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
    CC_x86_64_apple_darwin=x86_64-apple-darwin22.4-clang \
    AR_x86_64_apple_darwin=x86_64-apple-darwin22.4-ar \
    CC_aarch64_apple_darwin=aarch64-apple-darwin22.4-clang \
    AR_aarch64_apple_darwin=aarch64-apple-darwin22.4-ar \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
    CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
    AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar

# Debian ships the 64-bit loader as /usr/lib/wine/wine64 and puts the `wine`
# wrapper in a package that --no-install-recommends declines, so the smoke test
# has nothing on PATH to call. A two-line wrapper rather than pulling in that
# package's dependency tree, which is the larger half of an install that already
# takes twenty minutes.
#
# CAUTION: a symlink does not work here. wine finds the rest of its own
# installation relative to the path it was started from, so a link on PATH makes
# it look beside the link and answer "could not exec the wine loader" — which
# reads like a broken build rather than a wrong argv[0].
RUN if [ -x /usr/lib/wine/wine64 ] && ! command -v wine >/dev/null 2>&1; then \
        printf '#!/bin/sh\nexec /usr/lib/wine/wine64 "$@"\n' > /usr/local/bin/wine; \
        chmod 755 /usr/local/bin/wine; \
    fi
