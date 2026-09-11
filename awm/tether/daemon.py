"""Keep this host's tether binary running, and parented to this process.

One service folder, two kinds of host: the operator's node supervises
``tether-operator``, the public host supervises ``tether-relay``. Which one is
``paths.ROLE``, and nothing else about this module differs between them.

**The child is parented on purpose.** A tether that outlived its supervisor
would be exactly the persistence this tool refuses to have: sessions are sockets
held by tasks inside the child, so the child dying ends every one of them, and
the child dies with the service. It is put in its own session so the whole tree
can be signalled as a group, and ``PR_SET_PDEATHSIG`` closes the window where
this process dies between the fork and the child's first instruction.

The child is respawned from a supervised loop rather than at import, because a
service that blocks its startup on a subprocess is a service the gateway reaps
as broken — see AGENTS.md § *The ready-ASAP contract*.
"""

from __future__ import annotations

import logging
import os
import signal
import subprocess
import time
from pathlib import Path
from typing import Any

from awm.tether import paths

log = logging.getLogger("awm.tether.daemon")

#: How often the supervised loop asks whether the child is still there.
TICK_S = 5.0


def _preexec() -> None:  # pragma: no cover — runs in the forked child
    """Own session, die with the parent, and refuse to be orphaned.

    Order matters: ``setsid`` first so the tree is one signalable group, then
    the death signal, then a re-check — a parent that exited between the fork
    and ``prctl`` would leave a child nothing is ever going to signal.
    """
    os.setsid()
    try:
        import ctypes

        ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGTERM)  # PR_SET_PDEATHSIG
    except Exception:  # noqa: BLE001 — best effort; the group kill still works
        pass
    if os.getppid() == 1:
        os._exit(1)


class Child:
    """The one tether binary this host runs. Every method is safe to call."""

    def __init__(self) -> None:
        self._proc: subprocess.Popen | None = None
        self._started_at: float | None = None
        self._error: str | None = None

    # -- state --------------------------------------------------------------

    def alive(self) -> bool:
        return self._proc is not None and self._proc.poll() is None

    def snapshot(self) -> dict[str, Any]:
        """What this host can say about its child without asking the child."""
        binary = paths.role_binary()
        proc = self._proc
        return {
            "role": paths.ROLE,
            "binary": str(binary),
            "built": binary.is_file(),
            "built_at": (round(binary.stat().st_mtime) if binary.is_file() else None),
            "running": self.alive(),
            "pid": proc.pid if self.alive() else None,
            "exit_code": proc.poll() if proc else None,
            "uptime_s": (round(time.time() - self._started_at)
                         if self._started_at and self.alive() else None),
            "log": str(paths.DAEMON_LOG),
            "error": self._error,
        }

    # -- lifecycle ----------------------------------------------------------

    def _env(self) -> dict[str, str]:
        """The child's environment.

        Inherited wholesale, because what the child needs beyond the socket
        path — which relay this fleet uses, the bearer that reaches its
        authenticated half — lives in the workspace env file and is already in
        this process. Naming each variable here would mean editing this file
        every time one is added.
        """
        env = dict(os.environ)
        env["TETHER_CONTROL_SOCKET"] = str(paths.CONTROL_SOCKET)
        return env

    def start(self) -> dict[str, Any]:
        if self.alive():
            return self.snapshot() | {"action": "already-running"}

        binary = paths.role_binary()
        if not binary.is_file():
            self._error = (
                f"{binary} is not built on this host; run "
                f"awm/services/tether/install.sh where there is a toolchain, "
                f"or ship it here with ship-binaries.sh"
            )
            return self.snapshot() | {"action": "not-built"}

        paths.STATE_DIR.mkdir(parents=True, exist_ok=True)
        paths.STATE_DIR.chmod(0o700)
        # Appended, never truncated: this log is the only account of a crash
        # that happened between two `status` calls.
        out = open(paths.DAEMON_LOG, "ab", buffering=0)  # noqa: SIM115 — the child owns it
        try:
            self._proc = subprocess.Popen(
                [str(binary)],
                cwd=str(paths.SERVICE_DIR),
                env=self._env(),
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=subprocess.STDOUT,
                preexec_fn=_preexec,
            )
        except OSError as exc:
            self._error = f"{type(exc).__name__}: {exc}"
            return self.snapshot() | {"action": "failed"}
        finally:
            out.close()

        self._started_at = time.time()
        self._error = None
        log.info("tether: started %s (pid %s)", binary.name, self._proc.pid)
        return self.snapshot() | {"action": "started"}

    def stop(self, *, timeout: float = 10.0) -> dict[str, Any]:
        proc = self._proc
        if proc is None or proc.poll() is not None:
            self._proc = None
            return {"action": "not-running", "running": False}
        # The group, not the pid: the child may have spawned its own children
        # and signalling one of them leaves the rest holding the socket.
        self._signal_group(proc, signal.SIGTERM)
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self._signal_group(proc, signal.SIGKILL)
            proc.wait(timeout=5)
        self._proc = None
        self._started_at = None
        return {"action": "stopped", "running": False}

    def reconcile(self) -> dict[str, Any]:
        """Start the child if it is not there. Cheap; called on a loop."""
        if self.alive():
            return {"action": "none"}
        previous = self._proc.poll() if self._proc else None
        if previous not in (None, 0):
            log.warning("tether: the %s child exited with %s; restarting — see %s",
                        paths.ROLE, previous, paths.DAEMON_LOG)
        return self.start() | {"previous_exit": previous}

    @staticmethod
    def _signal_group(proc: subprocess.Popen, sig: int) -> None:
        try:
            os.killpg(os.getpgid(proc.pid), sig)
        except (ProcessLookupError, PermissionError):
            try:
                proc.send_signal(sig)
            except ProcessLookupError:
                pass


def tail(path: Path, lines: int) -> list[str]:
    """The last lines of a log file, or an empty list if there is no file yet."""
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return []
    return text.splitlines()[-max(lines, 1):]
