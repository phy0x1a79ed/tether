"""Run the Rust suite from the repo's own test runner.

Everything that matters about tether is in Rust: the invite-code vocabulary,
the handshake, the frames, the relay's limits. The repo's runner drives pytest
once per dist, so without this the whole tool would sit outside the one command
anybody runs before a deploy, and `run-tests.sh tether` would report PASS on
five assertions about a manifest.

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


@pytest.mark.skipif(shutil.which("cargo") is None, reason="no cargo on this host")
def test_the_rust_suite_passes():
    proc = subprocess.run(
        ["cargo", "test", "--manifest-path", str(MANIFEST), "--", "--quiet"],
        capture_output=True,
        text=True,
        timeout=1800,
    )
    # stdout carries the failing test names; a bare exit code sends whoever runs
    # this to a second command to find out what broke.
    assert proc.returncode == 0, proc.stdout + proc.stderr
