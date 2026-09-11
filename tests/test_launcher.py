"""The name the owner's launcher asks for, and the name the shipper writes.

These are two shell scripts that run on two different machines — the launcher
on whatever the owner is sitting at, the shipper on a box with a toolchain —
so they cannot share the expression that builds a client's file name. They
agree by being written to agree, and this is what says so.

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
SHIPPER = paths.SERVICE_DIR / "ship-binaries.sh"


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


def test_the_launcher_asks_for_the_name_the_shipper_writes(relay):
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

    # And the shipper's own expression for the same name, evaluated the same
    # way it is on a build box.
    shipped = subprocess.run(
        ["bash", "-c", 'echo "tether-linux-$(uname -m)"'],
        capture_output=True, text=True, timeout=30,
    ).stdout.strip()

    assert recorder.asked[0] == f"/bin/{shipped}"
    # Written out so the assertion above cannot pass by both sides being wrong
    # in the same way.
    assert shipped == f"tether-linux-{platform.machine()}"


def test_the_shipper_ships_the_launcher_to_the_mount_root():
    """The address a person is read out is the mount itself, so the launcher
    has to land at the asset the relay serves there, named `tether`."""
    text = SHIPPER.read_text()
    assert "launcher.sh" in text
    assert re.search(r"assets/tether\b", text), "the launcher does not land at the mount root"


def test_the_launcher_installs_nothing_and_says_where_it_left_the_one_file():
    """Four promises this scope makes, checked against the script that would
    be the place to break them."""
    text = LAUNCHER.read_text()
    for forbidden in ("launchctl", "systemctl", "crontab", "~/.bashrc",
                      "/usr/local/bin", "LaunchAgents"):
        assert forbidden not in text, f"the launcher mentions {forbidden}"
    # It runs the client in place of itself, so the terminal reaches the
    # consent prompt directly rather than through a wrapper.
    assert re.search(r'^exec "\$bin" "\$@"$', text, re.M)
    assert "delete it when you are done" in text
