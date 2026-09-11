"""The targets the build asks for, and the targets the container can link.

Two files decide whether a client can be produced at all. `artifacts.sh` names
the target that makes each artifact, and `container/Dockerfile` installs the
toolchains. They are read by different things — a shell loop and a container
build — and nothing else makes them agree.

The failure they prevent is the one that shipped a broken metasmith release: a
build that succeeds for the targets the image happens to carry, fails for the
rest, and leaves whatever was in the stage before to be shipped as if it were
fresh.
"""

from __future__ import annotations

import re

import pytest

from awm.tether import paths

pytestmark = [pytest.mark.unit, pytest.mark.smoke]

ARTIFACTS = paths.SERVICE_DIR / "artifacts.sh"
DOCKERFILE = paths.SERVICE_DIR / "container" / "Dockerfile"
DRIVER = paths.SERVICE_DIR / "build-clients.sh"


def table_triples() -> set[str]:
    text = ARTIFACTS.read_text()
    rows = re.findall(r'TETHER_(?:CLIENTS|SERVER)="(.*?)"', text, re.S)
    assert rows, "artifacts.sh no longer carries a table"
    return {line.split(":")[1] for row in rows for line in row.split() if line.strip()}


def test_every_target_the_table_names_is_installed_in_the_image():
    installed = set(re.findall(r"^\s+([a-z0-9_]+-[a-z0-9-]+)\s*\\?$",
                               DOCKERFILE.read_text(), re.M))
    missing = table_triples() - installed
    assert not missing, f"the image installs no toolchain for {sorted(missing)}"


def test_the_image_is_layered_onto_the_one_base_that_can_link_a_mac():
    """Replacing the base with a plain Rust image is the mistake that shipped
    three of four platforms broken, so the base is named and pinned in both
    files that mention it."""
    dockerfile = DOCKERFILE.read_text()
    driver = DRIVER.read_text()
    base = re.search(r"^ARG BASE=(\S+)$", dockerfile, re.M)
    assert base, "the Dockerfile no longer names a base"
    assert ":" in base.group(1), "the base is not pinned to a tag"
    assert f'BASE_IMAGE="{base.group(1)}"' in driver, \
        "build-clients.sh pulls a different base than the Dockerfile layers onto"


def test_the_windows_client_is_the_only_artifact_that_carries_an_extension():
    """Windows will not run a file without one, and every other system does not
    care. The launchers ask for exactly these names."""
    text = ARTIFACTS.read_text()
    names = [line.split(":")[0]
             for row in re.findall(r'TETHER_CLIENTS="(.*?)"', text, re.S)
             for line in row.split() if line.strip()]
    assert [n for n in names if n.endswith(".exe")] == ["tether-windows-x86_64.exe"]
    for name in names:
        if not name.endswith(".exe"):
            assert "." not in name, name


def test_the_build_tree_is_committed_rather_than_ignored():
    """A build definition nothing tracks is a build nobody else can run.

    The repository root ignores every directory called `build/`, which is why
    the container definition lives in `container/` and the artifact table lives
    at the service root. Nothing said so when it was wrong: `git status` simply
    did not mention the files.
    """
    import subprocess

    for path in (ARTIFACTS, DOCKERFILE, DRIVER):
        done = subprocess.run(["git", "check-ignore", "-q", str(path)],
                              cwd=path.parent, capture_output=True)
        assert done.returncode != 0, f"{path.name} is gitignored"
