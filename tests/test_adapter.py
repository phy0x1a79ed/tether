"""The two things the adapter answers itself, and the one thing it refuses.

Everything else here is a forward, and a forward is only worth testing against
a real daemon — which the Rust suite does. What is worth testing in Python is
the behaviour when there is no daemon to forward to, because that is the state
an operator is in exactly when they need the service to explain itself.
"""

from __future__ import annotations

import asyncio

import pytest

from awm.tether import hub_adapter, paths

pytestmark = [pytest.mark.unit, pytest.mark.smoke]


def test_the_verbs_that_must_work_without_a_daemon_are_not_forwards():
    """`status` says why the daemon is missing and `logs` shows it. Forwarding
    either would make both unanswerable in the one case they exist for."""
    assert hub_adapter.HANDLERS["status"] is hub_adapter.status
    assert hub_adapter.HANDLERS["logs"] is hub_adapter.logs


async def test_status_answers_with_no_daemon_running(tmp_path, monkeypatch):
    monkeypatch.setattr(paths, "CONTROL_SOCKET", tmp_path / "absent.sock")
    monkeypatch.setattr(paths, "ROLE", paths.OPERATOR)

    report = await hub_adapter.status({})

    assert report["child"]["role"] == paths.OPERATOR
    assert report["daemon"] == "unavailable"
    assert "logs" in report["hint"]


async def test_logs_reads_the_file_rather_than_the_daemon(tmp_path, monkeypatch):
    log = tmp_path / "daemon.log"
    log.write_text("first\nsecond\nthird\n")
    monkeypatch.setattr(paths, "DAEMON_LOG", log)

    assert (await hub_adapter.logs({"tail": 2}))["lines"] == ["second", "third"]
    # A host whose daemon has never started has no file, and that is not an
    # error — it is the answer.
    monkeypatch.setattr(paths, "DAEMON_LOG", tmp_path / "never.log")
    assert (await hub_adapter.logs({}))["lines"] == []


async def test_startup_schedules_the_child_rather_than_waiting_for_it(monkeypatch):
    """Two failures in one line of code, and both are silent.

    The gateway reaps a service that is slow to become ready, so starting the
    child inline would get this one killed on a slow disk. And the adapter
    awaits whatever ``on_start`` returns if it is awaitable — a Task is — so
    handing back the supervision task would block every inbound call behind a
    loop written never to finish.
    """
    ticks: list[int] = []
    monkeypatch.setattr(hub_adapter.CHILD, "reconcile", lambda: ticks.append(1))
    monkeypatch.setattr(hub_adapter.daemon, "TICK_S", 0.01)

    assert hub_adapter._on_start() is None

    try:
        await asyncio.sleep(0.05)
        assert ticks, "the child was never reconciled"
    finally:
        hub_adapter.SUPERVISION.cancel()


async def test_the_relay_host_refuses_the_verbs_that_mint_sessions(monkeypatch):
    """One folder runs on two kinds of host. On the public one a session verb
    is not a failure to reach a daemon — it is the wrong machine, and saying so
    is the difference between a five-second fix and a log dive."""
    monkeypatch.setattr(paths, "ROLE", paths.RELAY)

    answer = await hub_adapter.HANDLERS["invite"]({})

    assert answer["ok"] is False
    assert "operator" in answer["error"]
