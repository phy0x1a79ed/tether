"""The owner's launcher: the name it asks for, the source it asks first, and the
checksum it refuses a mismatch against.

Three scripts, on three kinds of machine: two launchers on whatever the owner is
sitting at, and the build on a box with a toolchain. They cannot share an
expression, so they agree by naming one table — ``artifacts.sh`` — and this is
part of what says they do.

A released launcher is not the source file. ``release.sh`` bakes the release tag
and every client's sha256 into the source and publishes the result; the source
itself carries empty values and refuses to run. So these tests render the source
the way the release does and run the rendered copy, except the one test that
checks the source refuses.

The failures they prevent: a 404 for a name the relay does not have, served to
an owner already asking for help; and, now that the client comes from a
different host than the launcher, a swapped or corrupted binary run without a
word.
"""

from __future__ import annotations

import hashlib
import http.server
import platform
import re
import shutil
import subprocess
import threading
from pathlib import Path

import pytest

pytestmark = pytest.mark.integration

# This repository's root, found from this file rather than from any installed
# package: the tests ship with the launchers they check and move with them.
REPO = Path(__file__).resolve().parent.parent

LAUNCHER = REPO / "launcher.sh"
LAUNCHER_PS1 = REPO / "launcher.ps1"
ARTIFACTS = REPO / "artifacts.sh"
RELAY_ROUTES = REPO / "crates" / "tether-relay" / "src" / "lib.rs"

TAG = "v9.9.9"


def client_names() -> list[str]:
    """Every client name the build stages, read from the one table that holds
    them."""
    block = re.search(r'TETHER_CLIENTS="(.*?)"', ARTIFACTS.read_text(), re.S)
    assert block, "artifacts.sh no longer carries a client table"
    return [line.split(":")[0] for line in block.group(1).split() if line.strip()]


def render(tmp_path, sums: dict[str, str], tag: str = TAG) -> Path:
    """Bake a tag and a checksum table into the source launcher the way
    release.sh does: copy up to the start marker, write the block, copy from the
    end marker on."""
    src = LAUNCHER.read_text().splitlines(keepends=True)
    start = next(i for i, ln in enumerate(src) if ln.startswith("# --- baked by release.sh"))
    end = next(i for i, ln in enumerate(src) if ln.startswith("# --- end baked ---"))
    sum_lines = "".join(f"{h}  {name}\n" for name, h in sums.items())
    block = (
        'TETHER_REPO="phy0x1a79ed/tether"\n'
        f'TETHER_TAG="{tag}"\n'
        f'TETHER_SUMS="\n{sum_lines}"\n'
    )
    out = tmp_path / "tether"
    out.write_text("".join(src[: start + 1]) + block + "".join(src[end:]))
    out.chmod(0o755)
    return out


class _Recorder(http.server.BaseHTTPRequestHandler):
    """A server that answers the paths in ``bodies`` and 404s the rest, and
    remembers everything it was asked for."""

    def do_GET(self):  # noqa: N802 — the base class names it
        self.server.asked.append(self.path)
        body = self.server.bodies.get(self.path)
        if body is None:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class _Server(http.server.HTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), _Recorder)
        self.bodies: dict[str, bytes] = {}
        self.asked: list[str] = []
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server_port}"


@pytest.fixture
def hosts():
    """Two stand-in hosts: one for the GitHub release, one for the relay."""
    github, relay = _Server(), _Server()
    try:
        yield github, relay
    finally:
        for s in (github, relay):
            s.shutdown()
            s.server_close()


# The owner's line says `sh` or `bash`, and which shell `sh` is depends on the
# machine. Each one this box has is a machine some owner is sitting at.
SHELLS = [s for s in (["bash"], ["dash"], ["busybox", "sh"]) if shutil.which(s[0])]

HOST_ASSET = f"tether-linux-{platform.machine()}"


def launch(shell, launcher, github, relay, *code, asset=None, no_github=False):
    # The launcher's directory is named for the client's hash, so a shared /tmp
    # would hand one test the client an earlier test fetched. Each launcher gets
    # the temporary directory beside it instead.
    tmp = launcher.parent / "tmp"
    tmp.mkdir(exist_ok=True)
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": "/tmp",
        "TMPDIR": str(tmp),
        "TETHER_GITHUB_BASE": github.base,
        "TETHER_RELAY": relay.base,
    }
    if asset:
        env["TETHER_ASSET"] = asset
    if no_github:
        env["TETHER_NO_GITHUB"] = "1"
    return subprocess.run(
        [*shell, str(launcher), *code],
        env=env, capture_output=True, text=True, timeout=60,
    )


def elf_header(machine: int) -> bytes:
    """The first twenty bytes of a Linux executable for one CPU, and nothing
    after them, so it can be recognised but never run."""
    return b"\x7fELF" + bytes(14) + machine.to_bytes(2, "little")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def test_the_unrendered_source_refuses():
    """A raw checkout ships no checksums, so it could only fetch unverified. It
    says so and stops rather than being what reaches an owner."""
    done = subprocess.run(
        ["sh", str(LAUNCHER), "acre", "anchor", "kettle"],
        env={"PATH": "/usr/bin:/bin", "HOME": "/tmp"},
        capture_output=True, text=True, timeout=60,
    )
    assert done.returncode == 1
    assert "launcher source" in done.stderr, done.stderr


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_github_is_tried_first_and_the_bytes_are_verified(hosts, tmp_path, shell):
    """The client comes from the release first. A body whose hash matches is
    accepted past the checksum; it then fails the run check, because a header
    with no program behind it cannot start — which is as far as a test without a
    real client can go, and proof the fetch and the verify both passed."""
    github, relay = hosts
    body = elf_header(0x3E)
    github.bodies[f"/{TAG}/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert f"fetched github" in done.stderr, done.stderr
    assert "would not start here" in done.stderr
    assert relay.asked == [], "the relay was asked despite a good GitHub download"


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_the_relay_is_the_fallback_when_github_cannot_be_reached(hosts, tmp_path, shell):
    """GitHub 404s every path here, so the launcher falls through to the relay
    and verifies the bytes it gets there just the same."""
    github, relay = hosts
    body = elf_header(0x3E)
    relay.bodies[f"/bin/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert "fetched relay" in done.stderr, done.stderr
    assert any(TAG in p for p in github.asked), "GitHub was never tried first"
    assert "would not start here" in done.stderr


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_second_run_reuses_the_client_and_fetches_nothing(hosts, tmp_path, shell):
    """One release leaves one directory. The second run finds the client the
    first one fetched, checks it against the same hash, and asks neither host."""
    github, relay = hosts
    body = elf_header(0x3E)
    github.bodies[f"/{TAG}/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    github.asked.clear()
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert "reusing the verified client" in done.stderr, done.stderr
    assert github.asked == [] and relay.asked == []
    assert [d.name for d in (tmp_path / "tmp").iterdir()] == [
        f"tether.{sha256(body)[:16]}"
    ]


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_client_altered_since_the_last_run_is_fetched_again(hosts, tmp_path, shell):
    """The hash is checked on every run, not only on the run that fetched."""
    github, relay = hosts
    body = elf_header(0x3E)
    github.bodies[f"/{TAG}/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    cached = tmp_path / "tmp" / f"tether.{sha256(body)[:16]}" / "tether"
    cached.write_bytes(b"altered")
    github.asked.clear()
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert "fetched github" in done.stderr, done.stderr
    assert cached.read_bytes() == body


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_directory_others_can_write_is_not_reused(hosts, tmp_path, shell):
    """Another user who makes the directory first could swap the client after
    it is verified, so a directory that is not private gets a fresh one."""
    github, relay = hosts
    body = elf_header(0x3E)
    github.bodies[f"/{TAG}/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    planted = tmp_path / "tmp" / f"tether.{sha256(body)[:16]}"
    planted.mkdir(parents=True)
    planted.chmod(0o777)
    (planted / "tether").write_bytes(body)
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert "fetched github" in done.stderr, done.stderr
    assert len(list((tmp_path / "tmp").iterdir())) == 2, "the planted directory was used"


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_wrong_checksum_is_refused_and_does_not_fall_back(hosts, tmp_path, shell):
    """A download that does not match is tampering or corruption, so it is
    refused outright and the relay is not asked to fix it."""
    github, relay = hosts
    served = elf_header(0x3E)
    github.bodies[f"/{TAG}/{HOST_ASSET}"] = served
    relay.bodies[f"/bin/{HOST_ASSET}"] = served
    # Bake the hash of a different body, so what GitHub serves cannot match.
    launcher = render(tmp_path, {HOST_ASSET: sha256(b"a different build entirely")})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle")
    assert done.returncode == 1
    assert "does not match the checksum" in done.stderr, done.stderr
    assert "nothing was run" in done.stderr
    assert relay.asked == [], "a checksum mismatch fell back to the relay"


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_client_for_another_machine_is_refused_before_it_runs(hosts, tmp_path, shell):
    """A verified download can still be the wrong architecture. The launcher
    reads the header and names which two machines these are, rather than running
    a binary this CPU cannot."""
    github, relay = hosts
    body = elf_header(0xB7)  # aarch64
    other = "tether-linux-aarch64"
    relay.bodies[f"/bin/{other}"] = body
    launcher = render(tmp_path, {other: sha256(body)})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle",
                  asset=other, no_github=True)
    assert done.returncode == 1
    assert "this machine is linux-x86_64" in done.stderr, done.stderr
    assert "is linux-aarch64" in done.stderr
    assert "TETHER_ASSET=tether-linux-aarch64" in done.stderr
    assert "nothing was run" in done.stderr


@pytest.mark.skipif(platform.machine() != "x86_64", reason="names an x86-64 host")
@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_a_web_page_is_not_mistaken_for_a_client(hosts, tmp_path, shell):
    github, relay = hosts
    body = b"<!doctype html><title>sign in</title>"
    relay.bodies[f"/bin/{HOST_ASSET}"] = body
    launcher = render(tmp_path, {HOST_ASSET: sha256(body)})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle",
                  no_github=True)
    assert done.returncode == 1
    assert "a web page, not a client" in done.stderr, done.stderr


@pytest.mark.parametrize("shell", SHELLS, ids=" ".join)
def test_the_launcher_asks_for_the_name_the_build_stages(hosts, tmp_path, shell):
    """With both sources 404ing, the launcher reports it could not download —
    and the path it asked the relay for is the name the build stages for this
    machine."""
    github, relay = hosts
    launcher = render(tmp_path, {HOST_ASSET: sha256(b"unused")})
    done = launch(shell, launcher, github, relay, "acre", "anchor", "kettle",
                  no_github=True)
    assert done.returncode != 0
    assert "could not download" in done.stderr, done.stderr
    assert relay.asked, "the launcher asked the relay for nothing"
    assert relay.asked[0] == f"/bin/{HOST_ASSET}"
    assert HOST_ASSET in client_names(), f"nothing staged is called {HOST_ASSET}"


def test_every_staged_client_has_a_launcher_that_can_ask_for_it():
    """A name nothing asks for is a name nobody can download."""
    asking = LAUNCHER.read_text() + LAUNCHER_PS1.read_text()
    for name in client_names():
        # Each launcher builds its names from parts, so what is checked is that
        # each part appears where a name is assembled.
        system, machine = name.replace(".exe", "").split("-")[1:3]
        assert system in asking, f"no launcher mentions {system}"
        assert machine in asking, f"no launcher mentions {machine}"


def test_the_relay_serves_both_launchers_at_the_mount_root():
    """The address a person is read out is the mount itself, so the relay has to
    serve each launcher as the asset a bare fetch of that mount returns."""
    served = RELAY_ROUTES.read_text()
    assert '&["tether"]' in served
    assert '&["tether.ps1"]' in served


def test_the_launcher_installs_nothing_and_says_what_it_left_behind():
    """Four promises this scope makes, checked against the script that would be
    the place to break them."""
    text = LAUNCHER.read_text()
    for forbidden in ("launchctl", "systemctl", "crontab", "~/.bashrc",
                      "/usr/local/bin", "LaunchAgents"):
        assert forbidden not in text, f"the launcher mentions {forbidden}"
    # It runs the client in place of itself, so the terminal reaches the consent
    # prompt directly rather than through a wrapper.
    assert re.search(r'^exec "\$bin" "\$@"$', text, re.M)
    # It names the whole directory rather than the client alone. There are two
    # files in it now — the client and the session record — and telling the
    # owner to delete one of them would leave the other.
    assert "delete $dir when you are done" in text


def test_the_powershell_launcher_installs_nothing_either():
    """The same promises, in the places Windows would break them."""
    text = LAUNCHER_PS1.read_text()
    for forbidden in ("schtasks", "New-Service", "Register-ScheduledTask",
                      "CurrentVersion\\Run", "Startup", "$PROFILE", "setx"):
        assert forbidden not in text, f"the PowerShell launcher mentions {forbidden}"
    # It runs the client and hands back whatever the client said, so a refused
    # session is a non-zero exit on Windows as it is everywhere else.
    assert re.search(r"^& \$bin @code$", text, re.M)
    assert re.search(r"^exit \$LASTEXITCODE$", text, re.M)
    assert "delete $dir when you are done" in text
