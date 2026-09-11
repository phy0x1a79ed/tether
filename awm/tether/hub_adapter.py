"""Hub adapter for the tether service — consent-gated remote assistance.

Registers with the gateway on the shared ``ServiceAdapter`` loop (register →
ready → serve → reconnect), so a tether session is driven from the same surface
as everything else: ``awm tether invite`` in a terminal, ``mcp__awm__tether``
from an agent.

This module is the only Python in the tool and it is thin on purpose. It
declares the verbs and forwards each one to a Rust daemon over a local socket.
Nothing here ever touches session bytes, holds a key, or decides who may
connect — those live in the daemon and the relay, where the vocabulary and the
handshake are one implementation shared by both ends.

Two verbs are answered here rather than forwarded, and for the same reason:
they have to work when the daemon does not. ``logs`` reads the log file, which
is the only account of a daemon that will not start; ``status`` reports what
this host can see of its child before asking the child anything.

Which child this host supervises is the host's role, not a runtime choice: the
operator's node runs the operator daemon, the public host runs the relay. See
``paths.ROLE``.

Run via ``run.sh`` (which the gateway spawns and respawns):
    python -m awm.tether.hub_adapter
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from awm.gatewayclient import ServiceAdapter, spawn_supervised

from awm.tether import control, daemon, paths

log = logging.getLogger("awm.tether.hub_adapter")

#: Every function carries an explicit ``tool`` name under a ``tether_`` prefix,
#: which is what decides the domain this service appears as: the gateway folds
#: the MCP surface by splitting the projected name on its **first** underscore.
#: The service name is one token for that reason and must stay one token.
API_MANIFEST: dict[str, Any] = {
    "description": (
        "Remote assistance with the owner's consent. You are the operator: you "
        "mint an invite, read the code to the person at the other machine, and "
        "they run one line and answer a prompt before anything connects. They "
        "watch everything you do and either side can cut it. Nothing is "
        "installed on their machine and nothing survives the session."
    ),
    "functions": [
        {
            "name": "invite",
            "tool": "tether_invite",
            "description": (
                "Mint an invite code and open a session for it at the relay. "
                "Returns the words to read out and the one-line command the "
                "owner runs on their machine. Only an operator can do this; "
                "the owner's side can only redeem. The session expires in "
                "minutes if nobody redeems it, and is destroyed after a few "
                "failed attempts."
            ),
            "params": [
                {"name": "words", "type": "number",
                 "description": "How many words in the code (default 2). More "
                                "words cost the owner nothing to type."},
            ],
            "timeout": 60,
        },
        {
            "name": "status",
            "tool": "tether_status",
            "description": (
                "Report this host's role, whether its binaries are built and "
                "at what commit, whether the daemon is running, and every live "
                "session with its age and which side is connected."
            ),
            "params": [],
        },
        {
            "name": "run",
            "tool": "tether_run",
            "description": (
                "Run one command on the owner's machine in a live session and "
                "return its output and exit status. The owner watches it "
                "happen; nothing here is hidden from them. One command at a "
                "time per session, and its input is closed from the start."
            ),
            "params": [
                {"name": "command", "type": "string",
                 "description": "The command to run."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot — the first "
                                "number of the invite code. Omit when only one "
                                "is live."},
            ],
            "timeout": 600,
        },
        {
            "name": "send",
            "tool": "tether_send",
            "description": (
                "Send a line of text to the person at the other keyboard. It "
                "appears on their screen. This is how you explain what you are "
                "about to do before you do it, and it reaches them while they "
                "are still deciding whether to let you in."
            ),
            "params": [
                {"name": "text", "type": "string",
                 "description": "What to say."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot. Omit when "
                                "only one is live."},
            ],
            "timeout": 60,
        },
        {
            "name": "cut",
            "tool": "tether_cut",
            "description": (
                "End a session from this side. The owner's client notices and "
                "exits. Either side can do this at any time."
            ),
            "params": [
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot. Omit when "
                                "only one is live."},
                {"name": "reason", "type": "string",
                 "description": "What to tell the owner. It appears on their "
                                "screen instead of a socket that went quiet."},
            ],
            "timeout": 60,
        },
        {
            "name": "logs",
            "tool": "tether_logs",
            "description": "Tail this host's tether daemon log.",
            "params": [
                {"name": "tail", "type": "number",
                 "description": "Lines to return (default 200)."},
            ],
        },
    ],
    "emitters": [],
    "sessions": [],
}

#: The verbs that act on a session, and so mean nothing on the relay host.
SESSION_VERBS = ("invite", "run", "send", "cut")

CHILD = daemon.Child()


# -- handlers ---------------------------------------------------------------


def _unreachable(exc: Exception) -> dict[str, Any]:
    """The same answer for every verb the daemon could not be asked."""
    return {
        "ok": False,
        "role": paths.ROLE,
        "daemon": "unavailable",
        "error": str(exc),
        "hint": f"no tether daemon is answering on this host; "
                f"`awm tether status` says why and `awm tether logs` shows it",
    }


def _forward(verb: str, timeout: float):
    async def handler(args: dict) -> dict:
        if paths.ROLE == paths.RELAY and verb in SESSION_VERBS:
            return {
                "ok": False,
                "role": paths.ROLE,
                "error": "this host is the relay; it carries sessions and "
                         "mints none. Run this verb on the operator's node.",
            }
        try:
            return await control.call(verb, args, timeout=timeout)
        except control.DaemonUnavailable as exc:
            return _unreachable(exc)
    return handler


async def status(args: dict) -> dict:
    """What this host knows, then what its daemon knows.

    Answered locally first so it still says something useful when the daemon is
    the thing that is wrong — which is the only time anybody reads it closely.
    """
    report: dict[str, Any] = {"ok": True, "child": CHILD.snapshot()}
    if paths.ROLE == paths.RELAY:
        # The relay's own status is behind its bearer and reachable only over
        # the network it serves. What this host can say is that it is running.
        return report | {"role": paths.ROLE, "sessions": []}
    try:
        report |= await control.call("status", {}, timeout=30)
    except control.DaemonUnavailable as exc:
        report |= _unreachable(exc)
    return report


async def logs(args: dict) -> dict:
    lines = args.get("tail")
    try:
        lines = int(lines) if lines is not None else 200
    except (TypeError, ValueError):
        lines = 200
    return {
        "ok": True,
        "path": str(paths.DAEMON_LOG),
        "lines": daemon.tail(paths.DAEMON_LOG, lines),
    }


# Every forwarded verb is the same forward, so the table is built rather than
# written out: a handler that diverged from its manifest entry is a bug nobody
# would see until the verb was called.
HANDLERS: dict[str, Any] = {
    fn["name"]: _forward(fn["name"], float(fn.get("timeout", 30)))
    for fn in API_MANIFEST["functions"]
}
HANDLERS["status"] = status
HANDLERS["logs"] = logs


async def _keep_child_running() -> None:
    """Start the child, and keep starting it. Never returns.

    Run under ``spawn_supervised`` rather than awaited from ``on_start``: the
    gateway reaps a service that is slow to become ready, so the only thing
    startup may do is arrange for this to happen, not wait for it.
    """
    while True:
        try:
            CHILD.reconcile()
        except Exception:  # noqa: BLE001 — a bad tick must not end the loop
            log.exception("tether: could not reconcile the %s child", paths.ROLE)
        await asyncio.sleep(daemon.TICK_S)


#: The supervision task, held so it can be inspected and stopped. Held rather
#: than discarded on principle, even though ``spawn_supervised`` is what makes
#: a dropped handle survivable.
SUPERVISION: asyncio.Task | None = None


def _on_start() -> None:
    """Arrange for the child to run, and return.

    Returning ``None`` is load-bearing. The adapter awaits whatever ``on_start``
    hands back if it is awaitable, and a Task is — so returning the supervision
    task would block initialisation on a loop written never to finish, and every
    inbound call would sit behind a gate that never opens.
    """
    global SUPERVISION
    SUPERVISION = spawn_supervised("tether-child", _keep_child_running)


async def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    log.info("tether: role=%s binaries=%s", paths.ROLE, paths.BIN_DIR)
    try:
        await ServiceAdapter("tether", API_MANIFEST, HANDLERS,
                             on_start=_on_start).run()
    finally:
        # The child dies with this process either way — that is what
        # PR_SET_PDEATHSIG is for. Stopping it here is what makes a clean
        # shutdown look clean in the log rather than like a kill.
        CHILD.stop()


if __name__ == "__main__":
    asyncio.run(main())
