"""Run the Rust suite from the repo's own test runner.

Everything that matters about tether is in Rust: the invite-code vocabulary,
the handshake, the frames, the relay's limits, the consent gate. The repo's
runner drives pytest once per dist, so without this the whole tool would sit
outside the one command anybody runs before a deploy, and `run-tests.sh tether`
would report PASS on five assertions about a manifest.

Skipped rather than failed where there is no toolchain, because the public host
has none by design and receives built artifacts instead.
"""

from __future__ import annotations

import shutil
import subprocess

import pytest

from awm.tether import paths

pytestmark = pytest.mark.integration

MANIFEST = paths.SERVICE_DIR / "rust" / "Cargo.toml"

#: Two runs, because the consent gate is the one thing that has to be tested
#: from both sides of a build flag. The default run proves the shipped binary
#: carries no way past the prompt and that a client with no terminal is refused;
#: the second enables the harness bypass so a whole session can run end to end
#: with nobody there to answer. Neither run alone says anything useful.
RUNS = [
    ["cargo", "test", "--manifest-path", str(MANIFEST)],
    [
        "cargo",
        "test",
        "--manifest-path",
        str(MANIFEST),
        "-p",
        "tether-owner",
        "--features",
        "test-consent-bypass",
    ],
]


@pytest.mark.skipif(shutil.which("cargo") is None, reason="no cargo on this host")
@pytest.mark.parametrize("command", RUNS, ids=["shipped", "with-consent-bypass"])
def test_the_rust_suite_passes(command):
    proc = subprocess.run(
        [*command, "--", "--quiet"],
        capture_output=True,
        text=True,
        timeout=1800,
    )
    # stdout carries the failing test names; a bare exit code sends whoever runs
    # this to a second command to find out what broke.
    assert proc.returncode == 0, proc.stdout + proc.stderr
