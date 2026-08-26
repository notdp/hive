"""Tests for `hive flow run`: script execution against the current team."""

import json

import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli


def test_flow_run_executes_script_in_team_context(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    from types import SimpleNamespace

    team = SimpleNamespace(name="t-x", workspace=str(tmp_path / "ws"))
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _t, required=True: ("t-x", team))

    out = tmp_path / "proof.json"
    script = tmp_path / "plan.py"
    script.write_text(
        "import json, pathlib\n"
        f"pathlib.Path({str(out)!r}).write_text(json.dumps({{'ran': True}}))\n"
    )

    result = runner.invoke(cli, ["flow", "run", str(script)])
    assert result.exit_code == 0, result.output
    assert json.loads(out.read_text()) == {"ran": True}


def test_flow_run_requires_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()

    def no_team(_t, required=True):
        from hive.cli import _fail

        _fail("no team bound")

    monkeypatch.setattr("hive.cli._resolve_scoped_team", no_team)
    script = tmp_path / "plan.py"
    script.write_text("raise AssertionError('script must not run without a team')\n")

    result = runner.invoke(cli, ["flow", "run", str(script)])
    assert result.exit_code == 1
    assert "no team bound" in result.output


def test_flow_run_surfaces_flow_errors_as_cli_failures(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    from types import SimpleNamespace

    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _t, required=True: ("t-x", SimpleNamespace(name="t-x")),
    )
    script = tmp_path / "plan.py"
    script.write_text(
        "from hive.flow import FlowError\n"
        "raise FlowError('member x never became ready')\n"
    )

    result = runner.invoke(cli, ["flow", "run", str(script)])
    assert result.exit_code == 1
    assert "member x never became ready" in result.output


def test_flow_run_missing_script_rejected_by_click(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    result = runner.invoke(cli, ["flow", "run", str(tmp_path / "nope.py")])
    assert result.exit_code == 2
