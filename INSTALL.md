# Installing the `tether` service

A consent-gated remote-assistance session between an **operator** and an
**owner**, bounced through a **relay** neither side has to be reachable from.
The service folder holds a thin Python adapter and, under `rust/`, the binaries
that do the work.

## Two roles, one folder

The same service runs on two kinds of host and `AWM_TETHER_ROLE` decides which:

| Role | Set on | Supervises | Needs |
|---|---|---|---|
| `operator` (default) | your own node | `tether-operator`, the daemon that mints invites and drives a session | a cargo toolchain, or shipped binaries |
| `relay` | the public host | `tether-relay`, which pairs two sockets and reads nothing | shipped binaries; the edge mount enabled |

The public host has no Rust toolchain and is not getting one. It receives built
artifacts from a build box, which is the same mechanism that serves the owner's
client, so one script does both.

## Install

    bash install.sh

Editable-installs `gatewayclient` and this service into the `awm` env (override
with `AWM_ENV=<name>`) and writes a gitignored `.runtime-env` sidecar baking
`AWM_PYTHON` = the env's absolute interpreter, so the gateway can respawn the
service under systemd's minimal PATH (where `mamba` is not present). It then
builds the Rust binaries if `cargo` is present, and says so plainly if it is
not — a host without a toolchain still installs, and reports the missing
binaries through `awm tether status`.

## Run

You never invoke the service by hand in normal operation. The gateway discovers
this folder (any folder with a `run.sh` under `awm/services/`), starts it with
`bash run.sh`, and injects the env vars the adapter reads:

| Env var | Set by | Meaning |
|---|---|---|
| `AWM_HUB_URL` | gateway | base URL of the running gateway |
| `AWM_SERVICE_NAME` | gateway | this service's name (= `tether`) |
| `AWM_SERVICE_ID` | gateway | assigned on respawn so reconnect targets the same control URL |
| `AWM_TETHER_ROLE` | host env file | `operator` (default) or `relay` |
| `AWM_TETHER_BIN` | host env file | where the built binaries live, when not the cargo target tree |

No auth — the registration handshake carries no token. The child dies with this
process, which is deliberate: a tether that outlived its supervisor would be the
persistence this tool refuses to have.
