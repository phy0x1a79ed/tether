#!/usr/bin/env bash
# Canonical install for the awm-tether service.
#
# Two halves. The Python half is the gateway adapter and installs like every
# other service. The Rust half is the three binaries under rust/, and it is
# skipped when there is no cargo on the box — the public host has no toolchain
# and is not getting one, so it receives built artifacts instead (see
# ship-binaries.sh). A host without cargo still installs cleanly and reports the
# missing binaries through `awm tether status` rather than failing here.
#
# Override the target env with AWM_ENV=<name>.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WS="$(git -C "$HERE" rev-parse --show-toplevel)"
ENV="${AWM_ENV:-awm}"
run() { echo "+ pip install -e $*"; mamba run -n "$ENV" pip install -e "$@"; }

run "$WS/awm/service_components/gatewayclient" --no-deps
run "$WS/awm/services/tether"

# Bake the target env's absolute interpreter into a gitignored `.runtime-env`
# sidecar so the hub supervisor can respawn this service under systemd's
# minimal PATH (no `mamba`).
PYBIN="$(mamba run -n "$ENV" python -c 'import sys; print(sys.executable)')"
printf 'AWM_PYTHON=%s\nAWM_ENV_BIN=%s\n' "$PYBIN" "$(dirname "$PYBIN")" \
    > "$HERE/.runtime-env"

if command -v cargo >/dev/null 2>&1; then
    echo "+ cargo build --release ($HERE/rust)"
    cargo build --release --manifest-path "$HERE/rust/Cargo.toml"
else
    echo "no cargo on this host — skipping the Rust binaries." >&2
    echo "this host must receive them from a build box; see ship-binaries.sh." >&2
fi

echo "Installed awm-tether into env '$ENV'."
