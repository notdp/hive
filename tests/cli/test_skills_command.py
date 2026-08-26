"""Tests for `hive skills get` / `hive skills list` — CLI-shipped spec serving.

The hive base skill is a thin discovery stub; the volatile protocol ships
inside the package and is fetched on demand via `hive skills get <name>`, so
it can never drift from the installed CLI version.
"""

import json

from hive.cli import cli


def test_skills_list_includes_core(runner):
    result = runner.invoke(cli, ["skills", "list"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "core" in payload["specs"]


def test_skills_get_core_serves_protocol(runner):
    result = runner.invoke(cli, ["skills", "get", "core"])
    assert result.exit_code == 0, result.output
    assert result.output.strip()


def test_skills_get_rejects_path_traversal(runner):
    result = runner.invoke(cli, ["skills", "get", "../etc/passwd"])
    assert result.exit_code != 0
    assert "invalid spec name" in result.output


def test_skills_get_unknown_lists_available(runner):
    result = runner.invoke(cli, ["skills", "get", "does-not-exist"])
    assert result.exit_code != 0
    assert "unknown spec" in result.output
    assert "core" in result.output  # error names the available specs


def test_skills_get_serves_core_and_orch_specs(runner):
    """Protocol content lives in CLI-served specs. The member contract (core)
    and the orch protocol are the only two the bootstrap dispatches."""
    listed = json.loads(runner.invoke(cli, ["skills", "list"]).output)["specs"]
    for name in ("core", "orch"):
        assert name in listed
        result = runner.invoke(cli, ["skills", "get", name])
        assert result.exit_code == 0, result.output
        assert result.output.strip()
    for retired in (
        "squad-orch", "squad-challenger", "squad-worker", "squad-validator",
        "duo-worker", "duo-validator",
    ):
        assert retired not in listed


def test_skills_get_serves_debug_and_advanced_routing(runner):
    """debug + advanced-routing were promoted from skill-home references to
    CLI-served specs, so core's pointers to them resolve through the same
    `hive skills get` interface instead of dangling at an unreachable file."""
    for name in ("debug", "advanced-routing"):
        result = runner.invoke(cli, ["skills", "get", name])
        assert result.exit_code == 0, result.output
        assert result.output.strip()
