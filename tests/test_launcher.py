"""The name the owner's launcher asks for, and the name the build writes.

Three scripts, on three kinds of machine: two launchers on whatever the owner
is sitting at, and the build on a box with a toolchain. They cannot share an
expression, so they agree by naming one table — ``artifacts.sh`` — and this is
what says they do.

The failure they prevent is quiet and badly timed: the relay answers 404 for a
name it does not have, and the owner, who is asking for help with something
already going wrong, sees a download fail for reasons nobody on the call can
diagnose.
"""

from __future__ import annotations

import http.server
import platform
import re
import subprocess
import threading

import pytest

from awm.tether import paths

pytestmark = pytest.mark.integration

LAUNCHER = paths.SERVICE_DIR / "launcher.sh"
LAUNCHER_PS1 = paths.SERVICE_DIR / "launcher.ps1"
SHIPPER = paths.SERVICE_DIR / "ship-binaries.sh"
ARTIFACTS = paths.SERVICE_DIR / "artifacts.sh"
RELAY_ROUTES = (
    paths.SERVICE_DIR / "rust" / "crates" / "tether-relay" / "src" / "lib.rs"
)


def client_names() -> list[str]:
    """Every client name the build stages, read from the one table that holds
    them."""
    block = re.search(r'TETHER_CLIENTS="(.*?)"', ARTIFACTS.read_text(), re.S)
    assert block, "artifacts.sh no longer carries a client table"
    return [line.split(":")[0] for line in block.group(1).split() if line.strip()]


class _Recorder(http.server.BaseHTTPRequestHandler):
    """Answers nothing and remembers what was asked for."""

    asked: list[str] = []

    def do_GET(self):  # noqa: N802 — the base class names it
        type(self).asked.append(self.path)
        self.send_error(404)

    def log_message(self, *args):
        pass


@pytest.fixture
def relay():
    _Recorder.asked = []
    server = http.server.HTTPServer(("127.0.0.1", 0), _Recorder)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", _Recorder
    finally:
        server.shutdown()
        server.server_close()


def test_the_launcher_asks_for_a_name_the_build_stages(relay):
    base, recorder = relay
    # The launcher, run exactly as `curl … | bash -s` runs it. It gets a 404,
    # which is the point: what is under test is the name in the request.
    done = subprocess.run(
        ["bash", str(LAUNCHER), "7", "anchor", "kettle"],
        env={"PATH": "/usr/bin:/bin", "TETHER_RELAY": base, "HOME": "/tmp"},
        capture_output=True, text=True, timeout=60,
    )
    assert done.returncode != 0
    assert "could not download" in done.stderr, done.stderr
    assert recorder.asked, "the launcher asked for nothing"

    asked = recorder.asked[0]
    assert asked.startswith("/bin/")
    assert asked[len("/bin/"):] in client_names(), f"nothing staged is called {asked}"
    # Written out so the assertion above cannot pass by both sides being wrong
    # in the same way.
    assert asked == f"/bin/tether-linux-{platform.machine()}"


def test_every_staged_client_has_a_launcher_that_can_ask_for_it():
    """A name nothing asks for is a name nobody can download."""
    asking = LAUNCHER.read_text() + LAUNCHER_PS1.read_text()
    for name in client_names():
        # Each launcher builds its names from parts, so what is checked is that
        # each part appears where a name is assembled.
        system, machine = name.replace(".exe", "").split("-")[1:3]
        assert system in asking, f"no launcher mentions {system}"
        assert machine in asking, f"no launcher mentions {machine}"


def test_the_shipper_ships_both_launchers_where_the_relay_looks():
    """The address a person is read out is the mount itself, so each launcher
    has to land at the asset the relay serves there."""
    shipped = SHIPPER.read_text()
    served = RELAY_ROUTES.read_text()
    assert "launcher.sh" in shipped
    assert "launcher.ps1" in shipped
    assert re.search(r"assets/tether\b", shipped), "the shell launcher misses the mount root"
    assert re.search(r"assets/tether\.ps1\b", shipped), "the PowerShell launcher is not shipped"
    # And the names the relay reads out of its asset directory.
    assert '&["tether"]' in served
    assert '&["tether.ps1"]' in served


def test_the_launcher_installs_nothing_and_says_what_it_left_behind():
    """Four promises this scope makes, checked against the script that would
    be the place to break them."""
    text = LAUNCHER.read_text()
    for forbidden in ("launchctl", "systemctl", "crontab", "~/.bashrc",
                      "/usr/local/bin", "LaunchAgents"):
        assert forbidden not in text, f"the launcher mentions {forbidden}"
    # It runs the client in place of itself, so the terminal reaches the
    # consent prompt directly rather than through a wrapper.
    assert re.search(r'^exec "\$bin" "\$@"$', text, re.M)
    # It names the whole directory rather than the client alone. There are two
    # files in it now — the client and the session record — and telling the
    # owner to delete one of them would leave the other.
    assert "delete $dir when you are done" in text


def test_the_powershell_launcher_installs_nothing_either():
    """The same four promises, in the places Windows would break them."""
    text = LAUNCHER_PS1.read_text()
    for forbidden in ("schtasks", "New-Service", "Register-ScheduledTask",
                      "CurrentVersion\\Run", "Startup", "$PROFILE", "setx"):
        assert forbidden not in text, f"the PowerShell launcher mentions {forbidden}"
    # It runs the client and hands back whatever the client said, so a refused
    # session is a non-zero exit on Windows as it is everywhere else.
    assert re.search(r"^& \$bin @code$", text, re.M)
    assert re.search(r"^exit \$LASTEXITCODE$", text, re.M)
    assert "delete $dir when you are done" in text
