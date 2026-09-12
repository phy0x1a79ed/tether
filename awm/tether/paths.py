"""Where tether's pieces live on a host, and which role this host plays.

One service folder runs on two kinds of box and they share no code path at
runtime: the operator's node supervises the operator daemon, the public host
supervises the relay. ``AWM_TETHER_ROLE`` decides, and defaults to operator
because that is what a workstation is.
"""

from __future__ import annotations

import os
from pathlib import Path

from awm.config import AWM_DIR

#: This service's folder — the one holding run.sh, install.sh and rust/.
SERVICE_DIR = Path(__file__).resolve().parents[2]

#: Where the built binaries are looked for. Overridable because the public host
#: has no toolchain: it receives artifacts into its own state directory rather
#: than having a cargo target tree inside the checkout, which a deploy's clean
#: of untracked files would delete.
BIN_DIR = Path(os.environ.get("AWM_TETHER_BIN")
               or SERVICE_DIR / "rust" / "target" / "release")

#: Per-host state: the daemon's control socket, its log, the build stamp.
STATE_DIR = AWM_DIR / "services" / "tether"

OPERATOR_BIN = BIN_DIR / "tether-operator"
RELAY_BIN = BIN_DIR / "tether-relay"
OWNER_BIN = BIN_DIR / "tether"

#: The operator daemon's control socket. Local, unauthenticated, and reachable
#: only by this user: the adapter is the only thing that speaks to it, and a
#: socket keeps one warm connection open across many verbs.
CONTROL_SOCKET = STATE_DIR / "operator.sock"

DAEMON_LOG = STATE_DIR / "daemon.log"

#: One file per session: what was run, what came back, and what the two people
#: said to each other. Beside this service's own state rather than inside a git
#: worktree, which is where every other service keeps its content and which
#: keeps a credential-adjacent record out of version control.
#:
#: Written only in the operator role. The relay promises to keep nothing about a
#: session and that promise stays literally true, because the relay role never
#: opens one of these.
SESSIONS_DIR = STATE_DIR / "sessions"

ROLE = os.environ.get("AWM_TETHER_ROLE", "operator").strip().lower()

OPERATOR = "operator"
RELAY = "relay"
ROLES = (OPERATOR, RELAY)


def role_binary() -> Path:
    """The binary this host's role supervises."""
    return RELAY_BIN if ROLE == RELAY else OPERATOR_BIN
