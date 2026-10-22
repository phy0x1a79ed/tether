# tether

A consent-gated remote-assistance session between an **operator** and an
**owner**, bounced through a public **relay** neither side has to be reachable
from. The operator mints a three-word invite code. The owner reads it into one
piped command, answers a prompt, and watches the session until either side cuts
it. Same category as TeamViewer, tmate and ngrok, self-hosted and ephemeral:
nothing survives the process, nothing starts at boot, nothing hides.

## Purpose & Contents

This repository holds everything that builds and ships tether: the Rust
workspace, the cross-build container, the owner's launchers, and the release
script. awm consumes it — the `tether` service there is a thin gateway adapter
that fetches the binaries this repository publishes.

What belongs here: the Rust source, the build and release tooling, the
launchers, and the protocol the binaries speak. What does not: the awm adapter
and its install contract, which live with awm at `awm/services/tether/`.

- `PROTOCOL.md` — one session end to end, and the agreements two separate
  binaries must both honour.
- `crates/` — the Rust workspace. `tether-proto` owns the invite code, the
  handshake and the frames. `tether-link` is the one dial both ends share.
  `tether-relay`, `tether-operator` and `tether-owner` are the three binaries.
- `artifacts.sh` — the single table of what ships, which `build-clients.sh`,
  `release.sh` and `build-macos.sh` all read. A target is added in one place.
- Script headers carry the rest. Read them before changing a build.

## The owner's side installs nothing

The owner runs one line and holds no key material:

    curl -fsSL https://nexus.tony-xy-liu.com/tether | sh -s acre anchor kettle

Windows has no shell that runs that script, so it has its own launcher at its
own address:

    & ([scriptblock]::Create((irm https://nexus.tony-xy-liu.com/tether/win))) acre anchor kettle

The launcher fetches the client for the machine it detects, verifies it, runs
it, and stops. It sets up no PATH entry, no login item, no service, and no
cron. The residue is one directory per release under the temporary directory,
named for the client's SHA-256 and holding the client and the session records,
both named in the consent prompt before the owner answers. A later run of the
same release checks the client already there against that hash and runs it
without downloading again.

The launcher fetches the client from this repository's release first and from
the relay only when GitHub is unreachable. It checks each download's SHA-256
against the value baked into the launcher before it runs anything, and refuses a
mismatch without falling back. `TETHER_NO_GITHUB=1` skips the GitHub source, for
a machine behind an allow-list.

An owner with no terminal, such as an agent in a sandbox, consents in two runs.
The first run shows the full prompt, prints three words and the exact line to
run again, and exits with status 3. The second run is that line with `--agree`
and the three words. `PROTOCOL.md` carries the window and the match rules.

## Building the clients

    ./build-clients.sh --image          # once per machine, and on a base bump
    ./build-clients.sh                  # every client, the relay and the operator

The owner downloads the client onto a machine nobody chose in advance, so every
binary is built in one container that pins its toolchains rather than against
whatever the build box carries: musl for Linux, which links statically,
osxcross for the two Apple targets, and mingw-w64 for Windows. The relay and the
operator are built the same way, because the box each runs on is not the box it
is built on. `container/Dockerfile` carries the toolchains.

`./build-clients.sh --host` is the inner loop: this machine's toolchain, this
machine's target, seconds rather than minutes. It marks the stage `local`, which
`release.sh` refuses.

The build stages into `dist/`, which is gitignored. A container that cannot link
a Darwin target builds the Mac client with `./build-macos.sh` on a Mac, into the
same stage.

## Cutting a release

    ./build-clients.sh                  # stage a whole `cross` build first
    ./release.sh v0.1.0                 # gate, render, tag, publish

`release.sh` refuses a dirty tree or a stage that is not a whole `cross` build.
It gates every artifact (magic bytes, a size floor, the executable bit, and the
absence of the consent-bypass marker), bakes each client's SHA-256 into the two
launchers, writes `SHA256SUMS`, tags the commit, and uploads every artifact to a
GitHub release. The release is created as a draft and published only once every
upload lands, so an owner's launcher never fetches from a half-populated
release. A release carries the five clients, the relay, the operator, both
rendered launchers, and `SHA256SUMS`.

## How awm runs a released tether

awm pins one tag in `awm/services/tether/RELEASE`. Each node's `fetch-release.sh`
reads the pin on every service start, fetches its role's binaries from the
matching GitHub release, verifies them against `SHA256SUMS`, and swaps them in.
awm and tether ship in lockstep — the operator decodes frames the clients send —
so a protocol change bumps `RELEASE` in the same awm change that needs it.
