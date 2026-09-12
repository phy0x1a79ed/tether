"""The buffer `drain` reads, the file both people agreed to, and the seam.

The interesting cases are all about losing things. A buffer that silently
shortens its answer produces a transcript with a hole in it that reads as though
nothing happened. A cursor carried across a daemon restart is either a wait for
events that are not coming or a whole history skipped as already seen. Both are
tested here, because neither is visible from a passing session.
"""

from __future__ import annotations

import base64
import json

import pytest

from awm.tether import hub_adapter, paths, stream

pytestmark = [pytest.mark.unit, pytest.mark.smoke]


def event(seq: int, type_: str = "owner.said", **fields) -> dict:
    return {"type": type_, "seq": seq, "at": 1757620391812, "slot": 7, **fields}


def test_a_reader_is_told_when_the_buffer_moved_past_its_cursor():
    journal = stream.Journal(max_events=4)
    for seq in range(10):
        journal.append(event(seq, text=f"line {seq}"))

    events, nxt, gap = journal.since(0)

    assert gap is not None, "a short answer must never stand in for a lost one"
    assert gap["from"] == 0
    assert gap["lost"] == gap["to"]
    assert events[0]["seq"] == gap["to"]
    assert nxt == 10


def test_a_reader_at_the_end_is_told_nothing_and_stays_there():
    journal = stream.Journal()
    for seq in range(3):
        journal.append(event(seq))

    events, nxt, gap = journal.since(3)

    assert events == []
    assert gap is None
    assert nxt == 3, "and does not go backwards"


def test_a_restarted_daemon_starts_the_cursor_again_rather_than_guessing():
    journal = stream.Journal()
    journal.reset(111)
    for seq in range(5):
        journal.append(event(seq))
    assert journal.head()["next"] == 5

    journal.reset(222)

    assert journal.head() == journal.head() | {"epoch": 222, "next": 0, "held": 0}


def test_filters_keep_one_session_and_one_kind():
    journal = stream.Journal()
    journal.append(event(0, "owner.said", text="hello"))
    journal.append(event(1, "task.output", task=1, stream="stdout", data="", bytes=0))
    journal.append({"type": "owner.said", "seq": 2, "slot": 9, "text": "elsewhere"})

    only_said, _, _ = journal.since(0, types=("owner.",))
    assert [e["seq"] for e in only_said] == [0, 2]

    only_seven, _, _ = journal.since(0, slot=7)
    assert [e["seq"] for e in only_seven] == [0, 1]


def test_output_split_across_chunks_is_whole_again_before_anyone_reads_it():
    """The reason output travels as bytes rather than text.

    A three-byte character cut in half by a chunk boundary is two invalid
    fragments. Joining before decoding is the only place that can be repaired,
    and it is why decoding happens here rather than in the daemon.
    """
    star = "★".encode()
    first, second = star[:2], star[2:]
    events = [
        event(0, "task.output", task=1, stream="stdout",
              data=base64.b64encode(first).decode(), bytes=len(first)),
        event(1, "task.output", task=1, stream="stdout",
              data=base64.b64encode(second).decode(), bytes=len(second)),
    ]

    merged = hub_adapter._decoded(events, raw=False)

    assert len(merged) == 1, "consecutive chunks of one stream are one thing"
    assert merged[0]["text"] == "★"
    assert merged[0]["bytes"] == 3


def test_two_streams_are_not_run_together():
    events = [
        event(0, "task.output", task=1, stream="stdout",
              data=base64.b64encode(b"out").decode(), bytes=3),
        event(1, "task.output", task=1, stream="stderr",
              data=base64.b64encode(b"err").decode(), bytes=3),
    ]

    merged = hub_adapter._decoded(events, raw=False)

    assert [e["stream"] for e in merged] == ["stdout", "stderr"]
    assert [e["text"] for e in merged] == ["out", "err"]


def test_raw_keeps_the_bytes_for_things_that_are_not_text():
    events = [
        event(0, "task.output", task=1, stream="screen",
              data=base64.b64encode(b"\x1b[2J").decode(), bytes=4),
    ]

    merged = hub_adapter._decoded(events, raw=True)

    assert base64.b64decode(merged[0]["data"]) == b"\x1b[2J"
    assert "text" not in merged[0]


def test_a_session_is_written_to_its_own_file_and_closed_when_it_ends(tmp_path):
    logs = stream.SessionLogs(tmp_path)

    logs.write(event(0, "session.phase", phase="open"))
    logs.write(event(1, "task.started", task=1, kind="command", command="df -h"))
    logs.write(event(2, "session.phase", phase="ended", ended="the owner ended it"))

    written = list(tmp_path.glob("*-slot7.jsonl"))
    assert len(written) == 1
    lines = [json.loads(line) for line in written[0].read_text().splitlines()]
    assert [line["type"] for line in lines] == [
        "session.phase", "task.started", "session.phase",
    ]
    assert lines[1]["command"] == "df -h"
    assert logs.path_for(7) is None, "an ended session closes its file"


def test_a_fact_belonging_to_no_session_is_written_to_no_session(tmp_path):
    logs = stream.SessionLogs(tmp_path)

    logs.write({"type": "daemon.started", "seq": 0, "at": 0, "build": "test"})

    assert list(tmp_path.glob("*.jsonl")) == []


def test_the_log_directory_is_this_user_alone(tmp_path):
    """It carries command output from somebody else's machine."""
    logs = stream.SessionLogs(tmp_path / "sessions")
    logs.write(event(0, "session.phase", phase="open"))

    written = next((tmp_path / "sessions").glob("*.jsonl"))
    assert oct((tmp_path / "sessions").stat().st_mode)[-3:] == "700"
    assert oct(written.stat().st_mode)[-3:] == "600"
    logs.close()


def test_the_sessions_directory_sits_beside_the_services_own_state():
    """Not inside a git worktree, where a record of somebody else's machine
    would end up under version control."""
    assert paths.SESSIONS_DIR.parent == paths.STATE_DIR
    assert paths.SESSIONS_DIR.name == "sessions"
