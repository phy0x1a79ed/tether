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

from awm.tether import control, daemon, paths, stream

log = logging.getLogger("awm.tether.hub_adapter")

#: What the daemon has told us, and where it went. Held at module level because
#: the watch task, the drain verb and ``status`` all read the same one.
JOURNAL = stream.Journal()
SESSION_LOGS = stream.SessionLogs()

#: Set once the adapter exists, so the watch task can reach ``emit``. The
#: adapter is constructed in ``main`` and the task is started from ``on_start``,
#: which runs inside it.
ADAPTER: ServiceAdapter | None = None

#: Where each caller last drained to, so the bare verb means "what is new" and
#: nobody has to type a number. Two surfaces draining the same session will each
#: consume what the other wanted, which is what the explicit cursor is for.
CURSORS: dict[str, int] = {}

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
        "installed on their machine. Both sides keep a record of what was "
        "done, which the owner is shown and agrees to before anything runs."
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
                "Start one command on the owner's machine in a live session. "
                "Returns a task id immediately and does NOT wait: the output "
                "and the exit status arrive on the session stream, so read "
                "them with `drain --until <task>`. The owner watches it "
                "happen; nothing here is hidden from them. Its input is closed "
                "from the start, so a command that reads until end of input "
                "does not wait for a keyboard."
            ),
            "params": [
                {"name": "command", "type": "string",
                 "description": "The command to run."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot — the first "
                                "number of the invite code. Omit when only one "
                                "is live."},
                {"name": "limit_s", "type": "number",
                 "description": "Stop the command after this many seconds. "
                                "Bounds one that would never end on a machine "
                                "you are a guest on."},
            ],
            "timeout": 60,
        },
        {
            "name": "shell",
            "tool": "tether_shell",
            "description": (
                "Open a terminal on the owner's machine, rather than running "
                "one command. This is the only way a full-screen program is "
                "usable, and the only way the owner sees it as the screen you "
                "are looking at rather than as escape codes. Type at it with "
                "`keys`, read it with `drain`, and end it with `close`. One "
                "terminal per session: the owner has one screen."
            ),
            "params": [
                {"name": "cols", "type": "number",
                 "description": "Terminal width (default 120)."},
                {"name": "rows", "type": "number",
                 "description": "Terminal height (default 40)."},
                {"name": "command", "type": "string",
                 "description": "What to run in it. Defaults to the owner's "
                                "own shell."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot."},
            ],
            "timeout": 60,
        },
        {
            "name": "keys",
            "tool": "tether_keys",
            "description": (
                "Type at a task. This talks to the PROGRAM, not to the person "
                "— use `send` to say something to them. Confusing the two "
                "types a sentence into somebody's shell."
            ),
            "params": [
                {"name": "task", "type": "number",
                 "description": "Which task, from `run` or `shell`."},
                {"name": "text", "type": "string",
                 "description": "What to type."},
                {"name": "enter", "type": "boolean",
                 "description": "Add a newline after the text."},
                {"name": "data", "type": "string",
                 "description": "Base64 bytes, for keys with no printable "
                                "form. Ctrl-C is `Aw==`. Use instead of "
                                "`text`, not with it."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot."},
            ],
            "timeout": 60,
        },
        {
            "name": "resize",
            "tool": "tether_resize",
            "description": (
                "Tell a task its terminal changed size. A full-screen program "
                "redraws itself to fit."
            ),
            "params": [
                {"name": "task", "type": "number", "description": "Which task."},
                {"name": "cols", "type": "number", "description": "New width."},
                {"name": "rows", "type": "number", "description": "New height."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot."},
            ],
            "timeout": 60,
        },
        {
            "name": "close",
            "tool": "tether_close",
            "description": (
                "Stop a task. This kills it rather than signalling end of "
                "input. The session carries on."
            ),
            "params": [
                {"name": "task", "type": "number", "description": "Which task."},
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot."},
            ],
            "timeout": 60,
        },
        {
            "name": "tasks",
            "tool": "tether_tasks",
            "description": (
                "What is open right now in a session: each task, what it is, "
                "how long it has been going and how much it has written."
            ),
            "params": [
                {"name": "code", "type": "string",
                 "description": "Which session, named by its slot."},
            ],
            "timeout": 60,
        },
        {
            "name": "send",
            "tool": "tether_send",
            "description": (
                "Say a line to the person at the other keyboard. It appears "
                "on their screen. This talks to the PERSON, not to a program "
                "— use `keys` to type at a task. This is how you explain what "
                "you are about to do before you do it, and it reaches them "
                "while they are still deciding whether to let you in."
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
            "name": "drain",
            "tool": "tether_drain",
            "description": (
                "Read what has happened in a session: commands and their "
                "output, how each ended, and both sides of the conversation. "
                "Answered from this host's own buffer, so it still works while "
                "the daemon is restarting. With no cursor it means 'what is "
                "new since I last asked', so you can call it bare. Use "
                "`until` with a task id from `run` to wait for that command to "
                "finish instead of polling for it."
            ),
            "params": [
                {"name": "cursor", "type": "number",
                 "description": "Read from here. Omit to continue from your "
                                "own last read."},
                {"name": "wait", "type": "number",
                 "description": "Seconds to wait for something to arrive "
                                "(default 0, at most 90)."},
                {"name": "until", "type": "number",
                 "description": "Return as soon as this task id has ended."},
                {"name": "limit", "type": "number",
                 "description": "Most events to return (default 500)."},
                {"name": "code", "type": "string",
                 "description": "Only this session, named by its slot."},
                {"name": "task", "type": "number",
                 "description": "Only this task."},
                {"name": "types", "type": "string",
                 "description": "Comma-separated kinds to keep, matched by "
                                "prefix: `owner.` for what the person said, "
                                "`task.` for what ran."},
                {"name": "raw", "type": "boolean",
                 "description": "Keep output as base64 rather than decoding it "
                                "to text. For bytes that are not text, such as "
                                "a terminal's escape sequences."},
            ],
            "timeout": 120,
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
    "emitters": [
        {
            "topic": "session",
            "description": (
                "Fires once per session event: a phase change, a task starting, "
                "a chunk of its output, how it ended, and either side of the "
                "conversation. Payload is the event, carrying {type, seq, at, "
                "slot} and its own fields — projected on /svc/tether/emit/"
                "session. Emitting is best effort, so a subscriber that must "
                "not miss anything reconciles against `drain` using the `seq` "
                "it last saw. The invite phrase is in no event: the slot is "
                "public, the phrase never leaves the two machines."
            ),
        },
    ],
    "sessions": [],
}

#: The verbs that act on a session, and so mean nothing on the relay host.
#: ``drain`` is not among them: it is answered here and truthfully returns an
#: empty buffer on a host that carries sessions rather than driving them.
SESSION_VERBS = ("invite", "run", "shell", "keys", "resize", "close",
                 "tasks", "send", "cut")

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
    report: dict[str, Any] = {
        "ok": True,
        "child": CHILD.snapshot(),
        # Where this host's own buffer stands, which is what `drain` reads and
        # is not the same thing as the daemon's. A caller comparing the two can
        # see whether the watch connection is keeping up.
        "stream": JOURNAL.head(),
    }
    if paths.ROLE == paths.RELAY:
        # The relay's own status is behind its bearer and reachable only over
        # the network it serves. What this host can say is that it is running.
        return report | {"role": paths.ROLE, "sessions": []}
    try:
        report |= await control.call("status", {}, timeout=30)
    except control.DaemonUnavailable as exc:
        report |= _unreachable(exc)
    return report


def _decoded(events: list[dict], raw: bool) -> list[dict]:
    """Turn chunks back into something a person can read.

    Output travels as bytes because a terminal's stream is escape sequences and
    a command's can split a character across a chunk boundary. Consecutive
    chunks of the same task and stream are joined before decoding, so a
    character that was split in transit is whole again by the time anyone sees
    it. That healing is only possible here, where the pieces are together.
    """
    import base64

    out: list[dict] = []
    for event in events:
        if event.get("type") != "task.output":
            out.append(event)
            continue
        last = out[-1] if out else None
        if (
            last is not None
            and last.get("type") == "task.output"
            and last.get("task") == event.get("task")
            and last.get("stream") == event.get("stream")
        ):
            last["_chunks"].append(event.get("data") or "")
            last["bytes"] = last.get("bytes", 0) + event.get("bytes", 0)
            continue
        joined = dict(event)
        joined["_chunks"] = [event.get("data") or ""]
        out.append(joined)

    for event in out:
        chunks = event.pop("_chunks", None)
        if chunks is None:
            continue
        # Decoded one chunk at a time and joined as bytes. Each chunk was
        # encoded on its own and carries its own padding, so joining the
        # encoded strings and decoding once would drop everything after the
        # first chunk that did not land on a three-byte boundary.
        blob = b""
        for chunk in chunks:
            try:
                blob += base64.b64decode(chunk)
            except Exception:  # noqa: BLE001 — one bad chunk is not the whole stream
                continue
        if raw:
            event["data"] = base64.b64encode(blob).decode("ascii")
            continue
        event.pop("data", None)
        event["text"] = blob.decode("utf-8", errors="replace")
    return out


async def drain(args: dict, as_: str | None = None) -> dict:
    """What has happened, from where this caller last looked.

    Answered here rather than forwarded, which is the point rather than a
    shortcut: the moment somebody most wants to know what a session did is the
    moment its daemon has just died.
    """
    def number(name: str, default: int | None = None) -> int | None:
        value = args.get(name)
        if value is None:
            return default
        try:
            return int(value)
        except (TypeError, ValueError):
            return default

    who = as_ or "-"
    cursor = number("cursor")
    if cursor is None:
        cursor = CURSORS.get(who)
    limit = max(1, min(number("limit", 500) or 500, 2000))
    hold = max(0.0, min(float(number("wait", 0) or 0), 90.0))
    until = number("until")
    slot = None
    code = args.get("code")
    if code:
        first = str(code).split()[0]
        try:
            slot = int(first)
        except ValueError:
            return {"ok": False, "error": f"`{code}` does not start with a slot number"}
    types = tuple(t.strip() for t in str(args.get("types") or "").split(",") if t.strip())

    if hold:
        found, _, _ = JOURNAL.since(cursor, limit=limit, slot=slot,
                                    task=number("task"), types=types)
        if not found:
            await JOURNAL.wait(hold, until=until)

    events, nxt, gap = JOURNAL.since(cursor, limit=limit, slot=slot,
                                     task=number("task"), types=types)
    CURSORS[who] = nxt
    head = JOURNAL.head()
    return {
        "ok": True,
        "epoch": head["epoch"],
        "cursor": nxt,
        "more": nxt < head["next"],
        "gap": gap,
        "events": _decoded(events, bool(args.get("raw"))),
    }


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
HANDLERS["drain"] = drain


async def _emit(topic: str, payload: dict) -> None:
    """Announce one event, if anything is listening. Never raises."""
    if ADAPTER is None:
        return
    await ADAPTER.emit(topic, payload)


async def _keep_watching() -> None:
    """Hold the daemon's event stream open for the life of this process."""
    await stream.watch_forever(JOURNAL, SESSION_LOGS, _emit)


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

#: The event stream's task, held for the same reason.
WATCHING: asyncio.Task | None = None


def _on_start() -> None:
    """Arrange for the child to run, and return.

    Returning ``None`` is load-bearing. The adapter awaits whatever ``on_start``
    hands back if it is awaitable, and a Task is — so returning the supervision
    task would block initialisation on a loop written never to finish, and every
    inbound call would sit behind a gate that never opens.
    """
    global SUPERVISION, WATCHING
    SUPERVISION = spawn_supervised("tether-child", _keep_child_running)
    # The events the child reports, on their way to the log, the topic and the
    # buffer `drain` reads. Supervised the same way and for the same reason: a
    # loop written never to finish must not be awaited from here.
    WATCHING = spawn_supervised("tether-watch", _keep_watching)


async def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    log.info("tether: role=%s binaries=%s", paths.ROLE, paths.BIN_DIR)
    global ADAPTER
    ADAPTER = ServiceAdapter("tether", API_MANIFEST, HANDLERS,
                             on_start=_on_start)
    try:
        await ADAPTER.run()
    finally:
        SESSION_LOGS.close()
        # The child dies with this process either way — that is what
        # PR_SET_PDEATHSIG is for. Stopping it here is what makes a clean
        # shutdown look clean in the log rather than like a kill.
        CHILD.stop()


if __name__ == "__main__":
    asyncio.run(main())
