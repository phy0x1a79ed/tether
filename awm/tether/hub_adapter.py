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

Which child this host supervises is the host's role, not a runtime choice:
the operator's node runs the operator daemon, the public host runs the relay.
See ``paths.ROLE``.

Run via ``run.sh`` (which the gateway spawns and respawns):
    python -m awm.tether.hub_adapter
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from awm.gatewayclient import ServiceAdapter

from awm.tether import control, paths

log = logging.getLogger("awm.tether.hub_adapter")

#: Every function carries an explicit ``tool`` name under a ``tether_`` prefix,
#: which is what decides the domain this service appears as: the gateway folds
#: the MCP surface by splitting the projected name on its **first** underscore.
#: The service name is one token for that reason and must stay one token.
API_MANIFEST: dict[str, Any] = {
    "functions": [
        {
            "name": "invite",
            "tool": "tether_invite",
            "description": (
                "Mint an invite code and open a session for it at the relay. "
                "Returns the two words to read out and the one-line command "
                "the owner runs on their machine. Only an operator can do "
                "this; the owner's side can only redeem. The session expires "
                "in minutes if nobody redeems it, and is destroyed after a few "
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
                "happen; nothing here is hidden from them."
            ),
            "params": [
                {"name": "command", "type": "string",
                 "description": "The command to run."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its invite code. Omit "
                                "when only one is live."},
            ],
            "timeout": 600,
        },
        {
            "name": "send",
            "tool": "tether_send",
            "description": (
                "Send a line of text to the person at the other keyboard. It "
                "appears on their screen. This is how you explain what you are "
                "about to do before you do it."
            ),
            "params": [
                {"name": "text", "type": "string",
                 "description": "What to say."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its invite code. Omit "
                                "when only one is live."},
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
                 "description": "Which session, named by its invite code. Omit "
                                "when only one is live."},
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


# -- handlers ---------------------------------------------------------------
#
# Every verb is the same forward, so the table is built rather than written out:
# a handler that diverged from its manifest entry is a bug nobody would see.


def _forward(verb: str, timeout: float):
    async def handler(args: dict) -> dict:
        try:
            return await control.call(verb, args, timeout=timeout)
        except control.DaemonUnavailable as exc:
            return {"ok": False, "role": paths.ROLE, "daemon": "unavailable",
                    "error": str(exc),
                    "hint": f"no tether daemon on this host; check "
                            f"{paths.role_binary()} and `awm tether logs`"}
    return handler


HANDLERS = {
    fn["name"]: _forward(fn["name"], float(fn.get("timeout", 30)))
    for fn in API_MANIFEST["functions"]
}


async def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    log.info("tether: role=%s binaries=%s", paths.ROLE, paths.BIN_DIR)
    await ServiceAdapter("tether", API_MANIFEST, HANDLERS).run()


if __name__ == "__main__":
    asyncio.run(main())
