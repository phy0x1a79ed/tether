# Installing the `tether` service

## Purpose & Contents

This file is the operator's contract for the `tether` service: what the folder
ships, which role a host plays, the environment each role reads, and how a host
with no toolchain gets its binaries.

What belongs here: the two roles, every environment variable and who sets it,
the install and shipping steps, and the failure each host shows when something
is missing. What does not belong here: the protocol, which is `PROTOCOL.md`,
and the verb surface, which `awm tether --help` renders from the live manifest.

## What this is

A consent-gated remote-assistance session between an **operator** and an
**owner**, bounced through a **relay** neither side has to be reachable from.
The operator mints an invite code. The owner reads it into one piped command,
answers a prompt, and watches the session until either side cuts it.

The folder holds a thin Python adapter and, under `rust/`, the three binaries
that do the work.

## Two roles, one folder

The same service runs on two kinds of host. `AWM_TETHER_ROLE` decides which.

| Role | Set on | Supervises | Needs |
|---|---|---|---|
| `operator` (default) | your own node | `tether-operator`, which mints invites and drives a session | a cargo toolchain, and the relay's bearer |
| `relay` | the public host | `tether-relay`, which pairs two sockets and reads nothing | shipped binaries, its own bearer, the edge mount |

The public host has no Rust toolchain and is not getting one. It receives built
artifacts from a build box, which is the same mechanism that serves the owner's
client, so one script does both.

## Install

    bash install.sh

The script editable-installs `gatewayclient` and this service into the `awm`
env. Override the env with `AWM_ENV=<name>`. It writes a gitignored
`.runtime-env` sidecar baking `AWM_PYTHON` = the env's absolute interpreter, so
the gateway can respawn the service under systemd's minimal PATH, where `mamba`
is absent. It then builds the Rust binaries when `cargo` is present, and says
so plainly when it is not. A host without a toolchain still installs, and
reports the missing binaries through `awm tether status`.

## Shipping binaries to a host that cannot build

    ./ship-binaries.sh [host]        # default: sirius

One trip carries three artifacts: the relay binary the host runs, the launcher
the owner pipes into a shell, and the Linux client the owner downloads. Read
the script's own header for the layout and the checks it runs.

**CAUTION** The macOS client is not built by that script and cannot be. Build
it on a Mac and ship it into the same assets directory. Its absence is visible
rather than silent: a Mac owner's launcher asks the relay for a name it does
not have, and says so.

## Environment

The gateway injects the first three. Everything else comes from the host's env
file, which is `<workspace>/.awm/env` on your own node and `/etc/awm/env` on
the public host.

| Env var | Read by | Meaning |
|---|---|---|
| `AWM_HUB_URL` | adapter | base URL of the running gateway |
| `AWM_SERVICE_NAME` | adapter | this service's name (= `tether`) |
| `AWM_SERVICE_ID` | adapter | assigned on respawn so reconnect targets the same control URL |
| `AWM_TETHER_ROLE` | adapter | `operator` (default) or `relay` |
| `AWM_TETHER_BIN` | adapter | where the built binaries live, when not the cargo target tree |
| `AWM_TETHER_ISSUE_TOKEN` | both roles | the bearer that makes a session operator-only |
| `AWM_TETHER_PORT` | relay | the loopback port it binds (default 12520) |
| `AWM_TETHER_ASSETS` | relay | the directory holding the launcher and the client downloads |
| `AWM_TETHER_MAX_SESSIONS` | relay | cap on live sessions |
| `AWM_TETHER_CLIENT_IP_HEADER` | relay | which header carries the owner's address (default `cf-connecting-ip`) |
| `AWM_EDGE_TETHER` | httpsfront | set `1` to mount the relay at `/tether` on the public edge |

The relay's port is declared once in `awm.config`, because two processes must
agree on it and neither owns it.

**CAUTION** `AWM_TETHER_ISSUE_TOKEN` is the whole of "only an operator may open
a session". Both hosts need **the same value**. The relay refuses to start
without one, which is the right direction: a relay that came up without a
bearer would accept sessions from anybody. The operator's daemon does start
without one, which is also the right direction: `awm tether status` is the verb
whose job is to explain why the others will not work, and a daemon that exited
could not answer it.

**CAUTION** The edge's path grammar and the relay's own parsers must agree
exactly. `awm/services/httpsfront/awm/httpsfront/tether.py` decides which paths
the public door opens, and it refuses everything else before consulting any
upstream. A grammar looser than `Slot::parse` lets a malformed slot reach the
relay. A grammar tighter than it turns a legitimate invite into a 404 that
names no cause. `awm/services/httpsfront/tests/test_tether_paths.py` writes
both boundaries out, so a change on either side has to change that file too.

## Run

You never invoke the service by hand in normal operation. The gateway discovers
this folder, starts it with `bash run.sh`, and supervises the child. No auth:
the registration handshake carries no token.

The child dies with this process, which is deliberate. A tether that outlived
its supervisor would be the persistence this tool refuses to have.

Run `awm tether --help` for the verbs. Two of them answer locally rather than
reaching the daemon, because they are the two that have to work when the daemon
is the thing that is wrong: `status` reports the child's process state before
merging whatever the daemon says, and `logs` reads the file.

## The owner's side installs nothing

The owner runs one line and holds no key material:

    curl -fsSL https://nexus.tony-xy-liu.com/tether | bash -s 7 anchor kettle

The launcher fetches the client for that machine, runs it, and stops. It sets
up no PATH entry, no login item, no launch agent, no service, and no cron. The
only residue is one downloaded file, in a directory of its own under the
temporary directory. The launcher prints that file's full path before running
it, so the owner can delete it without hunting for it.
