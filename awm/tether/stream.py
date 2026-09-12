"""The daemon's events, on their way to three places at once.

The adapter holds one long-lived connection to the daemon's ``watch`` verb, and
everything that arrives on it does three things:

1. it is appended to that session's log, which is the durable record both people
   agreed to when the owner answered the prompt;
2. it is emitted on this service's topic, for anything watching live;
3. it is kept in a bounded ring, so a caller that was not watching can ask what
   happened.

Those are not three mechanisms. They are one, and the reason is the CLI. No
service in this workspace pairs a topic with a stream a terminal can follow,
because topics exist to feed live pages while agents and the command line read
state back through ordinary verbs. So ``drain`` reads the ring, the log is what
makes the ring's contents worth keeping, and the topic is the fast path for
anyone able to hold a socket open. The log a person asked for and the push path
they asked for turn out to be the same thing seen from two sides.

**Emitting is best effort and the ring is not.** ``ServiceAdapter.emit`` returns
quietly when nothing is connected and drops on error, which is right for live
signalling and wrong as a delivery guarantee. Anything that must not be lost is
read back from here.

**A cursor belongs to a daemon.** The daemon stamps every stream with the epoch
it minted at startup. When that changes, this side resets rather than carrying a
number from one process into another, where it would either wait for events that
are not coming or skip a whole history as already seen.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import time
from pathlib import Path
from typing import Any, Awaitable, Callable, Iterable

from awm.tether import paths

log = logging.getLogger("awm.tether.stream")

#: How many events this side holds. Smaller than the daemon's own ring on
#: purpose: this one refills from that one after a restart, and a smaller buffer
#: upstream would turn a gap that should have been transient into a permanent
#: one.
MAX_EVENTS = 4096
MAX_BYTES = 8 * 1024 * 1024

#: How long a silent connection waits before deciding it is dead. The daemon
#: sends its own idle line well inside this, so silence this long is a socket
#: that stopped rather than a session that is quiet.
IDLE_DEADLINE_S = 90.0

#: How long a session file may grow before it stops growing. A record nobody can
#: open is not a record.
MAX_LOG_BYTES = 64 * 1024 * 1024


class Journal:
    """A bounded, cursor-addressed backlog of what the daemon reported."""

    def __init__(self, max_events: int = MAX_EVENTS, max_bytes: int = MAX_BYTES) -> None:
        self._events: list[dict] = []
        self._max_events = max_events
        self._max_bytes = max_bytes
        self._bytes = 0
        self._weights: list[int] = []
        self._first_kept = 0
        self._next = 0
        self._evicted = 0
        self._epoch: int | None = None
        self._connected = False
        self._since: float | None = None
        self._woken = asyncio.Event()

    # -- writing ------------------------------------------------------------

    def append(self, event: dict) -> None:
        seq = event.get("seq")
        if not isinstance(seq, int):
            return
        weight = len(event.get("data") or "") + 256
        self._events.append(event)
        self._weights.append(weight)
        self._bytes += weight
        self._next = max(self._next, seq + 1)
        while self._events and (
            len(self._events) > self._max_events or self._bytes > self._max_bytes
        ):
            dropped = self._events.pop(0)
            self._bytes -= self._weights.pop(0)
            self._first_kept = int(dropped["seq"]) + 1
            self._evicted += 1
        if self._events:
            self._first_kept = max(self._first_kept, int(self._events[0]["seq"]))
        self._woken.set()
        self._woken = asyncio.Event()

    def reset(self, epoch: int) -> None:
        """Start again, because this is a different daemon than last time."""
        if self._epoch is not None and self._epoch != epoch:
            log.info("tether: the daemon restarted; the event cursor starts again")
        self._epoch = epoch
        self._events.clear()
        self._weights.clear()
        self._bytes = 0
        self._first_kept = 0
        self._next = 0

    def connected(self, yes: bool) -> None:
        self._connected = yes
        self._since = time.time() if yes else None

    # -- reading ------------------------------------------------------------

    def since(
        self,
        cursor: int | None,
        *,
        limit: int = 500,
        slot: int | None = None,
        task: int | None = None,
        types: Iterable[str] = (),
    ) -> tuple[list[dict], int, dict | None]:
        """Events at or after ``cursor``, where to ask next, and any gap.

        The gap is never implied by a short answer. A caller that asked from
        before the ring's start is told so, because the alternative is a
        transcript with a hole in it that reads as though nothing happened.
        """
        start = self._first_kept if cursor is None else int(cursor)
        gap = None
        if start < self._first_kept:
            gap = {
                "type": "gap",
                "seq": self._first_kept,
                "from": start,
                "to": self._first_kept,
                "lost": self._first_kept - start,
                "note": "the buffer moved past this cursor; these events are gone",
            }
            start = self._first_kept

        wanted = tuple(types)
        out: list[dict] = []
        nxt = max(start, self._next if not self._events else start)
        for event in self._events:
            seq = int(event["seq"])
            if seq < start:
                continue
            if len(out) >= limit:
                break
            nxt = seq + 1
            if slot is not None and event.get("slot") != slot:
                continue
            if task is not None and event.get("task") != task:
                continue
            if wanted and not any(str(event.get("type", "")).startswith(w) for w in wanted):
                continue
            out.append(event)
        if not out:
            nxt = max(self._next, start)
        return out, nxt, gap

    async def wait(self, timeout: float, *, until: int | None = None) -> None:
        """Block until something lands, the named task ends, or time runs out.

        This is what makes a task id survivable from a terminal: a caller can
        ask for what happened and be answered when it has, rather than polling.
        """
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return
            waiter = self._woken.wait()
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(waiter, timeout=left)
            if until is None:
                return
            for event in reversed(self._events):
                if event.get("type") == "task.exited" and event.get("task") == until:
                    return

    def head(self) -> dict:
        return {
            "epoch": self._epoch,
            "next": self._next,
            "first_kept": self._first_kept,
            "evicted": self._evicted,
            "held": len(self._events),
            "connected": self._connected,
            "since_s": None if self._since is None else round(time.time() - self._since),
        }


class SessionLogs:
    """One file per session, opened on the first event that names its slot.

    Both people agreed to this when the owner answered the prompt, which is why
    it is written unconditionally rather than on request. It records the slot
    and never the phrase — the events themselves carry no phrase, so this is a
    property inherited rather than enforced again here.
    """

    def __init__(self, directory: Path | None = None) -> None:
        self._dir = directory if directory is not None else paths.SESSIONS_DIR
        self._open: dict[int, Any] = {}
        self._written: dict[int, int] = {}

    def write(self, event: dict) -> None:
        slot = event.get("slot")
        if not isinstance(slot, int):
            return
        handle = self._open.get(slot)
        if handle is None:
            handle = self._start(slot)
            if handle is None:
                return
        if self._written.get(slot, 0) >= MAX_LOG_BYTES:
            return
        line = json.dumps(event, separators=(",", ":")) + "\n"
        try:
            handle.write(line)
            handle.flush()
        except OSError:
            log.warning("tether: could not write the log for slot %s", slot, exc_info=True)
            self._close(slot)
            return
        self._written[slot] = self._written.get(slot, 0) + len(line)
        if event.get("type") == "session.phase" and event.get("phase") == "ended":
            self._close(slot)

    def path_for(self, slot: int) -> Path | None:
        handle = self._open.get(slot)
        return Path(handle.name) if handle is not None else None

    def _start(self, slot: int) -> Any:
        try:
            self._dir.mkdir(parents=True, exist_ok=True)
            os.chmod(self._dir, 0o700)
        except OSError:
            log.warning("tether: no session log directory at %s", self._dir, exc_info=True)
            return None
        stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        path = self._dir / f"{stamp}-slot{slot}.jsonl"
        try:
            handle = open(path, "a", encoding="utf-8")
        except OSError:
            log.warning("tether: could not open %s", path, exc_info=True)
            return None
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        self._open[slot] = handle
        self._written[slot] = 0
        log.info("tether: recording slot %s in %s", slot, path)
        return handle

    def _close(self, slot: int) -> None:
        handle = self._open.pop(slot, None)
        self._written.pop(slot, None)
        if handle is not None:
            with contextlib.suppress(OSError):
                handle.close()

    def close(self) -> None:
        for slot in list(self._open):
            self._close(slot)


Emit = Callable[[str, dict], Awaitable[None]]


async def watch_once(journal: Journal, logs: SessionLogs, emit: Emit) -> None:
    """One connection's worth of events. Returns when the socket closes."""
    reader, writer = await asyncio.open_unix_connection(str(paths.CONTROL_SOCKET))
    journal.connected(True)
    try:
        request = {"verb": "watch", "args": {"since": journal.head()["next"]}}
        writer.write(json.dumps(request).encode() + b"\n")
        await writer.drain()

        opening = await asyncio.wait_for(reader.readline(), timeout=IDLE_DEADLINE_S)
        if not opening:
            return
        head = json.loads(opening)
        epoch = head.get("epoch")
        if isinstance(epoch, int):
            if journal.head()["epoch"] != epoch:
                journal.reset(epoch)

        while True:
            raw = await asyncio.wait_for(reader.readline(), timeout=IDLE_DEADLINE_S)
            if not raw:
                return
            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                continue
            kind = event.get("type")
            if kind == "watch.idle":
                continue
            # Recorded before it is announced, so a subscriber that reacts by
            # asking `drain` always finds the event already there.
            journal.append(event)
            if paths.ROLE == paths.OPERATOR:
                logs.write(event)
            with contextlib.suppress(Exception):
                await emit("session", event)
    finally:
        journal.connected(False)
        writer.close()
        with contextlib.suppress(Exception):
            await writer.wait_closed()


async def watch_forever(journal: Journal, logs: SessionLogs, emit: Emit) -> None:
    """Hold the watch connection open for the life of the adapter.

    Never returns. The daemon is a supervised child that restarts, so losing
    this connection is ordinary rather than exceptional, and the backoff is
    there to keep a daemon that will not start from being asked constantly.
    """
    delay = 1.0
    while True:
        try:
            await watch_once(journal, logs, emit)
            delay = 1.0
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            pass
        except asyncio.TimeoutError:
            log.info("tether: the event stream went quiet; reconnecting")
        except Exception:  # noqa: BLE001 — a bad tick must not end the loop
            log.exception("tether: the event stream failed")
        await asyncio.sleep(delay)
        delay = min(delay * 2, 10.0)
