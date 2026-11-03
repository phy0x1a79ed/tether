"""The Windows launcher hands the owner their PowerShell back.

The owner runs ``launcher.ps1`` inside their own PowerShell, through one of two
lines, and an ``exit`` anywhere in it closes the window they typed into. These
tests type each line into a real PowerShell, then one more command, and check
that the second command ran: after a client that exits non-zero, after a
checksum the launcher refuses, and with no invite code at all.

PowerShell 7 on Linux stands in for Windows. It runs the same script engine,
and the launcher's Windows-only calls already sit inside a ``try``. The tests
use ``pwsh`` when this box has it, and otherwise the pinned PowerShell image,
so they run on any box with Docker.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

from test_launcher import LAUNCHER_PS1, TAG, _Server, sha256

pytestmark = pytest.mark.integration

IMAGE = "mcr.microsoft.com/powershell:7.4-ubuntu-22.04"
ASSET = "tether-windows-x86_64.exe"
MARK = "OWNER-PROMPT-STILL-HERE"

#: A client that says it ran and exits 3, so a test can tell the launcher ran
#: it and handed back its code.
CLIENT = b"#!/bin/sh\necho \"client ran with $*\" >&2\nexit 3\n"


def _pwsh() -> list[str] | None:
    if shutil.which("pwsh"):
        return ["pwsh"]
    if shutil.which("docker"):
        probe = subprocess.run(["docker", "image", "inspect", IMAGE],
                               capture_output=True, timeout=60)
        if probe.returncode == 0:
            return ["docker"]
    return None


PWSH = _pwsh()
needs_pwsh = pytest.mark.skipif(PWSH is None, reason=f"needs pwsh or the {IMAGE} image")


def render_ps1(tmp_path: Path, sums: dict[str, str], tag: str = TAG) -> bytes:
    """Bake a tag and a checksum table into the PowerShell source the way
    release.sh does."""
    src = LAUNCHER_PS1.read_text().splitlines(keepends=True)
    start = next(i for i, ln in enumerate(src) if ln.startswith("# --- baked by release.sh"))
    end = next(i for i, ln in enumerate(src) if ln.startswith("# --- end baked ---"))
    sum_lines = "".join(f"{h}  {name}\n" for name, h in sums.items())
    block = (
        "$TetherRepo = 'phy0x1a79ed/tether'\n"
        f"$TetherTag = '{tag}'\n"
        f"$TetherSums = @'\n{sum_lines}'@\n"
    )
    return ("".join(src[: start + 1]) + block + "".join(src[end:])).encode()


def owner(tmp_path: Path, relay: _Server, github: _Server, line: str,
          env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    """Type ``line`` into a PowerShell, then the command that proves the prompt
    survived it, and return what that PowerShell printed."""
    temp = tmp_path / "temp"
    temp.mkdir(exist_ok=True)
    script = (
        f"{line}\n"
        f"Write-Output \"{MARK} code=$LASTEXITCODE eap=$ErrorActionPreference\"\n"
    )
    full_env = {
        "TEMP": str(temp),
        "PROCESSOR_ARCHITECTURE": "AMD64",
        "TETHER_GITHUB_BASE": github.base,
        "TETHER_RELAY": relay.base,
        "HOME": "/tmp",
        **(env or {}),
    }
    if PWSH == ["pwsh"]:
        cmd = ["pwsh", "-NoProfile", "-NonInteractive", "-Command", script]
        run_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), **full_env}
    else:
        cmd = ["docker", "run", "--rm", "--network", "host",
               "--user", f"{os.getuid()}:{os.getgid()}",
               "-v", f"{tmp_path}:{tmp_path}"]
        for k, v in full_env.items():
            cmd += ["-e", f"{k}={v}"]
        cmd += [IMAGE, "pwsh", "-NoProfile", "-NonInteractive", "-Command", script]
        run_env = None
    return subprocess.run(cmd, env=run_env, capture_output=True, text=True, timeout=180)


@pytest.fixture
def hosts():
    github, relay = _Server(), _Server()
    try:
        yield github, relay
    finally:
        for s in (github, relay):
            s.shutdown()
            s.server_close()


def plant_client(tmp_path: Path, body: bytes) -> None:
    """Put a verified client where the launcher looks first.

    A download arrives without the execute bit, which Windows does not need and
    Linux does, so the run path is tested through the reuse branch.
    """
    d = tmp_path / "temp" / f"tether.{sha256(body)[:16]}"
    d.mkdir(parents=True)
    exe = d / "tether.exe"
    exe.write_bytes(body)
    exe.chmod(0o755)


SCRIPTBLOCK = "& ([scriptblock]::Create((irm {base}/win))) acre anchor kettle"
IEX = "$env:TETHER_CODE = 'acre anchor kettle'\nirm {base}/win | iex"
LINES = {"scriptblock": SCRIPTBLOCK, "iex": IEX}


@needs_pwsh
@pytest.mark.parametrize("route", LINES)
def test_the_prompt_survives_a_client_that_exits_non_zero(hosts, tmp_path, route):
    github, relay = hosts
    relay.bodies["/win"] = render_ps1(tmp_path, {ASSET: sha256(CLIENT)})
    plant_client(tmp_path, CLIENT)
    done = owner(tmp_path, relay, github, LINES[route].format(base=relay.base))
    out = done.stdout + done.stderr
    assert "client ran with acre anchor kettle" in out, out
    assert f"{MARK} code=3" in done.stdout, out


@needs_pwsh
@pytest.mark.parametrize("route", LINES)
def test_the_prompt_survives_a_refused_checksum(hosts, tmp_path, route):
    github, relay = hosts
    relay.bodies["/win"] = render_ps1(tmp_path, {ASSET: sha256(b"another build")})
    github.bodies[f"/{TAG}/{ASSET}"] = CLIENT
    done = owner(tmp_path, relay, github, LINES[route].format(base=relay.base))
    out = done.stdout + done.stderr
    assert "does not match the checksum" in out, out
    assert "nothing was run" in out, out
    assert f"{MARK} code=1" in done.stdout, out


@needs_pwsh
def test_the_prompt_survives_a_missing_code(hosts, tmp_path):
    github, relay = hosts
    relay.bodies["/win"] = render_ps1(tmp_path, {ASSET: sha256(CLIENT)})
    done = owner(tmp_path, relay, github, f"irm {relay.base}/win | iex")
    out = done.stdout + done.stderr
    assert "no invite code" in out, out
    assert f"{MARK} code=1" in done.stdout, out


@needs_pwsh
def test_the_iex_route_leaves_the_owners_session_as_it_was(hosts, tmp_path):
    """The launcher sets Stop for itself. The owner's own preference stays
    Continue after it, and none of its variables leak into their session."""
    github, relay = hosts
    relay.bodies["/win"] = render_ps1(tmp_path, {ASSET: sha256(CLIENT)})
    plant_client(tmp_path, CLIENT)
    line = IEX.format(base=relay.base) + "\nWrite-Output \"leak=[$TetherTag$bin]\""
    done = owner(tmp_path, relay, github, line)
    out = done.stdout + done.stderr
    assert "leak=[]" in done.stdout, out
    assert f"{MARK} code=3 eap=Continue" in done.stdout, out


@needs_pwsh
def test_the_unrendered_source_refuses_and_returns(hosts, tmp_path):
    github, relay = hosts
    relay.bodies["/win"] = LAUNCHER_PS1.read_bytes()
    done = owner(tmp_path, relay, github, SCRIPTBLOCK.format(base=relay.base))
    out = done.stdout + done.stderr
    assert "launcher source" in out, out
    assert f"{MARK} code=1" in done.stdout, out


def test_the_launcher_never_calls_exit():
    """Any `exit` closes the owner's PowerShell. Comments may say the word."""
    code = [ln for ln in LAUNCHER_PS1.read_text().splitlines()
            if not ln.lstrip().startswith("#")]
    assert not [ln for ln in code if ln.strip().startswith("exit")
                or " exit " in f" {ln.strip()} "], code
