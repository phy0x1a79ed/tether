"""Talk to the operator daemon over its local control socket.

The daemon is the long-lived half: it holds a session open across many verbs, so
the adapter can stay a thin forwarder and a verb costs a local round trip rather
than a fresh negotiation with the relay.

One request per connection, JSON in and one JSON line out. There is no auth,
because the socket is created inside this user's state directory with no group
or world access, and the only caller is the adapter in the same process tree.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

from awm.tether import paths

#: Generous, because a verb may be waiting on the owner to answer a prompt at
#: their keyboard. The manifest declares matching per-verb ceilings.
DEFAULT_TIMEOUT_S = 600.0


class DaemonUnavailable(RuntimeError):
    """The control socket is not there, or nothing is listening on it."""


async def call(verb: str, args: dict[str, Any] | None = None, *,
               timeout: float = DEFAULT_TIMEOUT_S) -> dict[str, Any]:
    """Send one verb to the daemon and return its reply.

    Raises DaemonUnavailable when the daemon is not up, which the adapter turns
    into a reported state rather than a crash: a service that cannot register
    because its child is missing can never tell anyone why.
    """
    try:
        reader, writer = await asyncio.open_unix_connection(str(paths.CONTROL_SOCKET))
    except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
        raise DaemonUnavailable(str(exc)) from exc

    try:
        writer.write(json.dumps({"verb": verb, "args": args or {}}).encode() + b"\n")
        await writer.drain()
        line = await asyncio.wait_for(reader.readline(), timeout=timeout)
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:  # noqa: BLE001 — the daemon may have gone first
            pass

    if not line:
        raise DaemonUnavailable("daemon closed the connection without replying")
    return json.loads(line)


def available() -> bool:
    """Whether the control socket exists. Not proof anything is listening."""
    return paths.CONTROL_SOCKET.is_socket()
