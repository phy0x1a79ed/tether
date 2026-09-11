"""What the manifest projects onto the MCP/CLI surface.

Worth a test rather than a comment because the failure is silent and only
visible after a deploy: the gateway folds a projected tool name by splitting on
its **first** underscore, so a name that carries an extra underscore becomes a
domain and a verb that both read as nonsense. Nothing errors; the surface is
simply wrong, and the docs that quote it are wrong with it.
"""

from __future__ import annotations

import pytest

from awm.tether.hub_adapter import API_MANIFEST, HANDLERS

pytestmark = [pytest.mark.unit, pytest.mark.smoke]

FUNCTIONS = API_MANIFEST["functions"]


def test_every_function_projects_into_a_single_domain():
    for fn in FUNCTIONS:
        domain, _, verb = fn["tool"].partition("_")
        assert domain == "tether", (fn["name"], fn["tool"])
        assert verb, fn["tool"]


def test_the_projected_verb_matches_the_internal_name():
    """A divergence routes fine but makes `awm tether <verb>` and the handler
    table disagree, which is the kind of drift nobody notices."""
    for fn in FUNCTIONS:
        assert fn["tool"] == f"tether_{fn['name']}", fn


def test_every_declared_function_has_a_handler():
    assert {fn["name"] for fn in FUNCTIONS} == set(HANDLERS)


def test_the_session_verbs_outlast_the_client_default():
    """They wait on a person: the owner answering a consent prompt, or a
    command they are watching run. The default 30s client ceiling would abort
    mid-session and hand the caller a timeout instead of a result."""
    for fn in FUNCTIONS:
        if fn["name"] in {"invite", "run", "send", "cut"}:
            assert fn.get("timeout", 0) >= 60, fn["name"]


def test_the_service_folder_name_is_the_domain():
    """The gateway discovers this service by its folder name and registers under
    it, so a folder renamed to carry a hyphen or an underscore would split into
    a domain nobody asked for while every tool name here kept saying `tether`."""
    from awm.tether import paths

    assert paths.SERVICE_DIR.name == "tether"
