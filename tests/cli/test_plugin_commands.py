import json

import pytest

from hive.cli import cli


def _frontmatter_keys(text: str) -> set[str]:
    lines = text.splitlines()
    assert lines[0] == "---"
    keys: set[str] = set()
    for line in lines[1:]:
        if line == "---":
            return keys
        keys.add(line.split(":", 1)[0])
    raise AssertionError("frontmatter terminator not found")


def test_plugin_list_does_not_offer_retired_cvim_or_fork(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    listed = runner.invoke(cli, ["plugin", "list", "--json"])
    assert listed.exit_code == 0
    names = {item["name"] for item in json.loads(listed.output)}
    assert "cvim" not in names
    assert "fork" not in names
    assert {"notify", "code-review"}.issubset(names)


@pytest.mark.parametrize("retired", ["cvim", "fork"])
def test_plugin_enable_rejects_retired_plugins(runner, configure_hive_home, retired):
    configure_hive_home(tmux_inside=False)
    result = runner.invoke(cli, ["plugin", "enable", retired])
    assert result.exit_code != 0
    assert "core hive capability" in result.output


def test_plugin_enable_code_review_materializes_skill(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"
    codex_home = hive_home.parent / ".codex"
    claude_home = hive_home.parent / ".claude"

    enabled = runner.invoke(cli, ["plugin", "enable", "code-review"])

    assert enabled.exit_code == 0
    assert "Plugin 'code-review' enabled." in enabled.output
    assert "skills: code-review" in enabled.output
    for skill_root in (
        factory_home / "skills",
        claude_home / "skills",
        codex_home / "skills",
    ):
        assert (skill_root / "code-review").is_symlink()

    # --- SKILL.md ---
    skill = hive_home / "plugins" / "installed" / "code-review" / "skills" / "code-review" / "SKILL.md"
    assert skill.exists()
    skill_text = skill.read_text()
    assert _frontmatter_keys(skill_text) == {"name", "description"}
    assert "depends on the `hive` skill" in skill_text
    assert "load the `hive` skill" in skill_text
    assert "3 Reviewer + Evidence Verification" in skill_text
    assert "reviewer-a" in skill_text
    assert "Evidence" in skill_text
    assert "hive layout" in skill_text

    # --- Stage files exist ---
    stages_dir = hive_home / "plugins" / "installed" / "code-review" / "skills" / "code-review" / "stages"
    expected_stages = [
        "1-pipeline-orch.md", "1-review-reviewer.md", "1-verify-verifier.md",
        "2-fix-orch.md", "2-fix-verify.md", "3-summary-orch.md",
    ]
    for name in expected_stages:
        assert (stages_dir / name).exists(), f"Missing stage file: {name}"

    # --- S1 pipeline ---
    s1_pipeline = (stages_dir / "1-pipeline-orch.md").read_text()
    assert "hive spawn reviewer-a" in s1_pipeline
    assert "hive spawn reviewer-b" in s1_pipeline
    assert "hive spawn reviewer-c" in s1_pipeline
    assert "hive kill reviewer-a" in s1_pipeline
    assert "hive spawn verifier-a" in s1_pipeline
    assert "hive layout main-vertical" in s1_pipeline
    assert "hive send orch" in s1_pipeline
    assert "idle" in s1_pipeline.lower()
    assert 'RUN_NAME="cr-${RUN_ID}"' in s1_pipeline
    assert 'ARTIFACT_DIR="$WORKSPACE/artifacts/${RUN_NAME}"' in s1_pipeline
    assert 'STATE_DIR="$WORKSPACE/state/${RUN_NAME}"' in s1_pipeline
    assert 'Run Artifact Root: $ARTIFACT_DIR' in s1_pipeline
    assert 'Run State Root: $STATE_DIR' in s1_pipeline
    assert 'rm -rf "$WORKSPACE/artifacts" "$WORKSPACE/state" "$WORKSPACE/events"' not in s1_pipeline
    assert "## Decision Boundaries" in s1_pipeline
    assert "Agent 自主完成（不问人）：" in s1_pipeline
    assert "必须升级给人类：" in s1_pipeline
    assert "gh pr comment（对外可见）" in s1_pipeline
    assert "review 范围超出原始 diff" in s1_pipeline

    # --- S1 reviewer ---
    s1_reviewer = (stages_dir / "1-review-reviewer.md").read_text()
    assert "File:" in s1_reviewer
    assert "Code:" in s1_reviewer
    assert "Verify:" in s1_reviewer

    # --- S1 verifier ---
    s1_verifier = (stages_dir / "1-verify-verifier.md").read_text()
    assert "confirmed" in s1_verifier
    assert "fabricated" in s1_verifier

    # --- S2 fix ---
    s2_fix = (stages_dir / "2-fix-orch.md").read_text()
    assert "hive spawn fixer" in s2_fix
    assert "hive spawn checker" in s2_fix
    assert "hive kill fixer" in s2_fix
    assert 'printf \'%s\' "$ROUND" > "$STATE_DIR/s2-round"' in s2_fix
    assert "Run Artifact Root: $ARTIFACT_DIR" in s2_fix
    assert "Run State Root: $STATE_DIR" in s2_fix

    # --- S3 summary ---
    s3_summary = (stages_dir / "3-summary-orch.md").read_text()
    assert "review-summary.md" in s3_summary
    assert 'printf \'%s\' "$ARTIFACT_DIR/review-summary.md" > "$STATE_DIR/review-summary-artifact"' in s3_summary

    enabled_json = runner.invoke(cli, ["plugin", "enable", "code-review", "--json"])
    assert enabled_json.exit_code == 0
    assert json.loads(enabled_json.output)["enabled"] is True


def test_plugin_enable_code_review_does_not_touch_existing_user_review_skill(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"

    user_skill_dir = factory_home / "skills" / "review"
    user_skill_dir.mkdir(parents=True, exist_ok=True)
    user_skill_file = user_skill_dir / "SKILL.md"
    user_skill_file.write_text("---\nname: review\ndescription: my custom review skill\n---\n")

    enabled = runner.invoke(cli, ["plugin", "enable", "code-review"])

    assert enabled.exit_code == 0
    assert "skills: code-review" in enabled.output
    assert "Warning: skipped skill(s) review" not in enabled.output
    assert user_skill_file.read_text().startswith("---\nname: review\ndescription: my custom review skill")
    assert not user_skill_dir.is_symlink()
    assert (factory_home / "skills" / "code-review").is_symlink()


def test_plugin_reenable_preserves_user_skill_that_replaced_old_plugin_symlink(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"

    enabled = runner.invoke(cli, ["plugin", "enable", "code-review"])
    assert enabled.exit_code == 0

    review_skill = factory_home / "skills" / "review"
    if review_skill.is_symlink():
        review_skill.unlink()
    elif review_skill.is_dir():
        import shutil
        shutil.rmtree(review_skill)
    review_skill.mkdir(parents=True, exist_ok=True)
    (review_skill / "SKILL.md").write_text("---\nname: review\ndescription: user custom\n---\n")

    reenabled = runner.invoke(cli, ["plugin", "enable", "code-review"])
    assert reenabled.exit_code == 0
    assert review_skill.exists()
    assert not review_skill.is_symlink()
    assert (review_skill / "SKILL.md").read_text().startswith("---\nname: review\ndescription: user custom")


def test_plugin_enable_notify_is_pure_toggle_without_files_or_hooks(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"
    codex_home = hive_home.parent / ".codex"
    claude_home = hive_home.parent / ".claude"

    enabled = runner.invoke(cli, ["plugin", "enable", "notify"])

    assert enabled.exit_code == 0
    assert "Plugin 'notify' enabled." in enabled.output
    # notify plugin is a pure toggle: the sidecar idle watcher reads its
    # enabled state. Enable installs no commands, skills, or hooks.
    assert "commands:" not in enabled.output
    assert "skills:" not in enabled.output
    assert not (factory_home / "commands" / "notify").exists()
    assert not (claude_home / "commands" / "notify.md").exists()
    assert not (codex_home / "skills" / "notify").exists()

    settings_path = factory_home / "settings.json"
    settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
    assert "hooks" not in settings


def test_plugin_enable_disable_outputs_codex_restart_hint(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)

    enabled = runner.invoke(cli, ["plugin", "enable", "notify"])
    disabled = runner.invoke(cli, ["plugin", "disable", "notify"])

    hint = "existing Codex panes may not reload plugin settings dynamically"
    assert enabled.exit_code == 0
    assert disabled.exit_code == 0
    assert hint in enabled.output
    assert hint in disabled.output


@pytest.fixture
def capture_exec(monkeypatch):
    calls: list[tuple[str, list[str]]] = []

    def fake_execvp(file: str, argv: list[str]) -> None:
        calls.append((file, list(argv)))
        raise SystemExit(0)

    monkeypatch.setattr("hive.cli.os.execvp", fake_execvp)
    return calls


@pytest.mark.parametrize("hive_command, expected_mode", [("cvim", "cvim"), ("vim", "vim")])
def test_cvim_cli_exec_core_binary_with_mode(
    runner,
    configure_hive_home,
    capture_exec,
    hive_command,
    expected_mode,
):
    configure_hive_home(tmux_inside=True)

    result = runner.invoke(cli, [hive_command, "--", "--extra", "arg1"])
    assert result.exit_code == 0, result.output

    assert len(capture_exec) == 1
    command_name, argv = capture_exec[0]
    assert command_name == "bash"
    assert argv[0] == "bash"
    assert argv[1].endswith("core_assets/cvim/bin/cvim-command")
    assert argv[2] == expected_mode
    assert argv[3:] == ["--extra", "arg1"]


@pytest.fixture
def capture_fork_subprocess(monkeypatch):
    popen_calls: list[list[str]] = []
    run_calls: list[list[str]] = []

    class _FakePopen:
        def __init__(self, argv, **kwargs):
            popen_calls.append(list(argv))

    def _fake_run(argv, **kwargs):
        run_calls.append(list(argv))

        class _R:
            returncode = 0
        return _R()

    monkeypatch.setattr("hive.cli.subprocess.Popen", _FakePopen)
    monkeypatch.setattr("hive.cli.subprocess.run", _fake_run)
    return popen_calls, run_calls


@pytest.mark.parametrize("hive_command, expected_split", [("vfork", "v"), ("hfork", "h")])
def test_fork_split_spawns_background_hive_fork(
    runner, configure_hive_home, capture_fork_subprocess, monkeypatch, hive_command, expected_split
):
    configure_hive_home(tmux_inside=True)
    monkeypatch.setenv("TMUX_PANE", "%42")

    popen_calls, run_calls = capture_fork_subprocess
    result = runner.invoke(cli, [hive_command, "--", "--extra", "arg1"])
    assert result.exit_code == 0, result.output

    assert popen_calls == [["hive", "fork", "-s", expected_split, "--extra", "arg1"]]
    assert len(run_calls) == 1
    escape_argv = run_calls[0]
    assert escape_argv[:3] == ["tmux", "run-shell", "-b"]
    assert "%42" in escape_argv[3]
    assert "Escape" in escape_argv[3]


def test_fork_split_without_tmux_pane_skips_escape(
    runner, configure_hive_home, capture_fork_subprocess, monkeypatch
):
    configure_hive_home(tmux_inside=True)
    monkeypatch.delenv("TMUX_PANE", raising=False)

    popen_calls, run_calls = capture_fork_subprocess
    result = runner.invoke(cli, ["vfork"])
    assert result.exit_code == 0, result.output

    assert popen_calls == [["hive", "fork", "-s", "v"]]
    assert run_calls == []
