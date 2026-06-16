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
    # protocol body + the heredoc/artifact idiom relocated here from the stub
    assert "## 消息机制" in result.output
    assert "--artifact - <<'EOF'" in result.output


def test_skills_get_rejects_path_traversal(runner):
    result = runner.invoke(cli, ["skills", "get", "../etc/passwd"])
    assert result.exit_code != 0
    assert "invalid spec name" in result.output


def test_skills_get_unknown_lists_available(runner):
    result = runner.invoke(cli, ["skills", "get", "does-not-exist"])
    assert result.exit_code != 0
    assert "unknown spec" in result.output
    assert "core" in result.output  # error names the available specs


def test_skills_list_includes_duo_and_squad(runner):
    result = runner.invoke(cli, ["skills", "list"])
    assert result.exit_code == 0, result.output
    specs = json.loads(result.output)["specs"]
    assert {"core", "duo", "squad"} <= set(specs)


def test_skills_get_duo_serves_worker_and_validator(runner):
    """duo is the shared atom: worker (producer) + validator (reviewer),
    with the coordinator left abstract so squad can bind it."""
    result = runner.invoke(cli, ["skills", "get", "duo"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "worker" in out and "validator" in out
    assert "协调者" in out  # coordinator kept abstract in the atom
    assert "successState" in out  # handoff schema lives in the atom


def test_skills_get_squad_composes_duo(runner):
    """squad is the orchestration delta (orch + challenger); it must compose
    the duo atom by reference, not re-inline the worker/validator kernel."""
    result = runner.invoke(cli, ["skills", "get", "squad"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "orch" in out and "challenger" in out
    assert "duo" in out  # references the atom, no duplication
    assert "spawn-duo" in out


def test_skills_get_core_includes_challenge_stance(runner):
    result = runner.invoke(cli, ["skills", "get", "core"])
    assert result.exit_code == 0, result.output
    assert "挑战立场" in result.output


def test_skills_get_core_includes_ask_user_idiom(runner):
    """问用户走 runtime 的阻塞式工具(AskUserQuestion / request_user_input),
    不是打印一行就往下走 —— 这条 idiom 随 core spec 下发。"""
    result = runner.invoke(cli, ["skills", "get", "core"])
    assert result.exit_code == 0, result.output
    assert "AskUserQuestion" in result.output


def test_skills_get_bypasses_stale_skill_gate(runner, monkeypatch):
    """`skills get` is the recovery/bootstrap path — it must serve specs even
    when the installed stub is stale, otherwise the stub→`skills get core`
    handoff would deadlock behind the drift gate."""
    monkeypatch.setattr(
        "hive.cli.skill_sync.diagnose_hive_skill",
        lambda *_args, **_kwargs: {"state": "stale"},
    )
    monkeypatch.setattr("hive.cli._current_pane_agent_cli", lambda: "claude")
    result = runner.invoke(cli, ["skills", "get", "core"])
    assert result.exit_code == 0, result.output
    assert "## 消息机制" in result.output


def test_skills_get_serves_role_specs(runner):
    """Role content lives in CLI-served specs now (no per-role SKILL.md). Every
    role a spawn dispatches `hive skills get <role>` for must be fetchable."""
    listed = json.loads(runner.invoke(cli, ["skills", "list"]).output)["specs"]
    for role in (
        "squad-orch", "squad-challenger", "squad-worker", "squad-validator",
        "duo-worker", "duo-validator",
    ):
        assert role in listed
        result = runner.invoke(cli, ["skills", "get", role])
        assert result.exit_code == 0, result.output
        assert result.output.strip()


def test_skills_get_serves_debug_and_advanced_routing(runner):
    """debug + advanced-routing were promoted from skill-home references to
    CLI-served specs, so core's pointers to them resolve through the same
    `hive skills get` interface instead of dangling at an unreachable file."""
    for name in ("debug", "advanced-routing"):
        result = runner.invoke(cli, ["skills", "get", name])
        assert result.exit_code == 0, result.output
        assert result.output.strip()
