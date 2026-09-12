"""The half of `tether-dev.sh` that is not shell.

Before the branch is promoted there is no `awm tether` on this node, so the
daemon's control socket is driven directly. That socket is the same one the
gateway adapter uses and it speaks the same two shapes: one line of JSON in and
one line out for every verb, and `watch`, which answers with a line per event
until the reader goes away.

The interesting part is `run`. The daemon answers it with a task id and does not
wait, because output is a fact on a stream rather than the return value of a
call. That is right for an agent, which wants two auditable steps and a
transcript, and wrong for a person at a terminal who typed one command and wants
an answer. So this does the second half: it submits, follows the stream from the
cursor the daemon handed back, prints what the command wrote, and stops at its
exit status. The awm surface stays honest and the driver stays comfortable.

Delete this with `tether-dev.sh` once the branch is promoted.
"""

from __future__ import annotations

import base64
import json
import socket
import sys


def connect(path: str) -> socket.socket:
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(620)
    s.connect(path)
    return s


def ask(path: str, verb: str, args: dict | None = None) -> dict:
    """One verb, one answer."""
    s = connect(path)
    try:
        s.sendall(json.dumps({"verb": verb, "args": args or {}}).encode() + b"\n")
        line = s.makefile("rb").readline()
    finally:
        s.close()
    if not line:
        raise SystemExit("the daemon closed the connection without answering")
    return json.loads(line)


def follow(path: str, since: int = 0, idle: int = 30):
    """Every event from `since` onwards, as they happen. Never returns.

    `idle` is how long the daemon waits before saying it is still there, which
    is also how `drain` knows it has reached the end: there is no other way to
    tell "nothing more" from "nothing yet" on a stream that never closes.
    """
    s = connect(path)
    # No deadline: this is the one request that is answered for as long as the
    # reader cares to listen.
    s.settimeout(None)
    request = {"verb": "watch", "args": {"since": since, "idle_ping_s": idle}}
    s.sendall(json.dumps(request).encode() + b"\n")
    stream = s.makefile("rb")
    try:
        for raw in stream:
            if raw.strip():
                yield json.loads(raw)
    finally:
        s.close()


def show(event: dict) -> None:
    """One event, as a line a person would want to read."""
    kind = event.get("type")
    slot = event.get("slot")
    where = f"[{slot}]" if slot is not None else "[-]"
    if kind == "task.output":
        text = base64.b64decode(event.get("data") or "").decode("utf-8", "replace")
        sys.stdout.write(text)
        sys.stdout.flush()
    elif kind == "task.started":
        print(f"{where} $ {event.get('command') or event.get('kind')}  (task {event['task']})")
    elif kind == "task.exited":
        code, signal = event.get("exit_code"), event.get("signal")
        how = f"exit {code}" if code is not None else (
            f"killed by signal {signal}" if signal is not None else "ended without saying how")
        print(f"{where} task {event['task']}: {how}")
    elif kind == "owner.said":
        print(f"{where} them: {event['text']}")
    elif kind == "operator.said":
        print(f"{where} you: {event['text']}")
    elif kind == "session.phase":
        ended = event.get("ended")
        print(f"{where} {event['phase']}" + (f": {ended}" if ended else ""))
    elif kind == "session.miss":
        print(f"{where} somebody tried the wrong words ({event['misses']} so far)")
    elif kind == "gap":
        print(f"--- {event['lost']} events were dropped before this point ---")
    elif kind not in {"watch.open", "watch.idle"}:
        print(f"{where} {kind} {json.dumps({k: v for k, v in event.items() if k not in ('type', 'seq', 'at', 'slot')})}")


def die(answer: dict) -> None:
    if answer.get("ok") is False or "error" in answer:
        print(answer.get("error", answer), file=sys.stderr)
        raise SystemExit(1)


def main() -> None:
    path, verb, arg = sys.argv[1], sys.argv[2], (sys.argv[3] if len(sys.argv) > 3 else "")

    if verb == "invite":
        answer = ask(path, "invite")
        die(answer)
        print()
        print("Run this on the owner machine, in a real terminal:")
        print()
        print("  " + answer["command"])
        print()
        print(f"slot {answer['slot']}, live for {answer.get('expires_in', '?')} seconds")
        return

    if verb == "run":
        started = ask(path, "run", {"command": arg})
        die(started)
        task, cursor = started["task"], started["cursor"]
        # The half the awm surface leaves to the caller: follow from where the
        # daemon said this task begins, and stop when it ends.
        for event in follow(path, cursor):
            if event.get("task") == task or event.get("type") == "gap":
                show(event)
            if event.get("type") == "task.exited" and event.get("task") == task:
                return
            if event.get("type") == "session.phase" and event.get("phase") == "ended":
                print("the session ended before the command did", file=sys.stderr)
                raise SystemExit(1)
        return

    if verb == "watch":
        for event in follow(path, int(arg) if arg else 0):
            show(event)
        return

    if verb == "drain":
        # Everything held, then stop — the difference from `watch` is only that
        # this one does not wait for more.
        for event in follow(path, int(arg) if arg else 0, idle=1):
            if event.get("type") == "watch.idle":
                return
            show(event)
        return

    if verb == "shell":
        answer = ask(path, "shell", {"cols": 120, "rows": 40} if not arg else {"command": arg})
        die(answer)
        print(f"terminal open as task {answer['task']} "
              f"({answer['cols']}x{answer['rows']}) — type at it with "
              f"`keys '{answer['task']}:...'`, watch it with `watch`")
        return

    if verb == "keys":
        # `<task>:<what>`, with `^C` as the one control character worth typing
        # by hand. Anything else that is not text goes through the awm verb.
        task, _, what = arg.partition(":")
        args = {"task": int(task)}
        if what == "^C":
            args["data"] = base64.b64encode(b"\x03").decode()
        else:
            args |= {"text": what, "enter": True}
        die(ask(path, "keys", args))
        print("typed")
        return

    if verb in {"close", "tasks", "status", "say", "cut"}:
        args = {
            "close": {"task": int(arg)} if arg else {},
            "tasks": {},
            "status": {},
            "say": {"text": arg},
            "cut": {"reason": arg or "the session is over"},
        }[verb]
        answer = ask(path, "send" if verb == "say" else verb, args)
        print(json.dumps(answer, indent=4, sort_keys=True))
        return

    print(f"unknown verb: {verb}", file=sys.stderr)
    raise SystemExit(1)


if __name__ == "__main__":
    main()
