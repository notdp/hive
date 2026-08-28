import json
import os

from hive import agent_cli, tmux


def test_normalize_command_strips_path_and_aliases():
    assert agent_cli.normalize_command("/usr/local/bin/claude") == "claude"
    assert agent_cli.normalize_command("claude-code") == "claude"
    assert agent_cli.normalize_command("CODEX") == "codex"
    assert agent_cli.normalize_command("claude.exe") == "claude"
    assert agent_cli.normalize_command("/opt/homebrew/bin/claude.exe") == "claude"
    assert agent_cli.normalize_command("") == ""


def test_member_role_classifies_agents_and_shells():
    assert agent_cli.member_role("claude") == "agent"
    assert agent_cli.member_role("codex") == "agent"
    assert agent_cli.member_role("grok") == "agent"
    assert agent_cli.member_role("zsh") == "terminal"
    assert agent_cli.member_role("python3") == "terminal"


def test_profiles_use_expected_skill_commands():
    assert agent_cli.get_profile("claude").skill_cmd == "/{name}"
    assert agent_cli.get_profile("codex").skill_cmd == "${name}"
    assert agent_cli.get_profile("grok").skill_cmd == "/{name}"


def test_grok_profile_forks_through_the_hive_launcher():
    profile = agent_cli.get_profile("/usr/local/bin/grok")
    assert profile.name == "grok"
    assert profile.fork_cmd.format(session_id="sess-1") == (
        "hive grok --resume sess-1 --fork-session"
    )


def test_detect_profile_for_pane_uses_title_and_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "\u2733 Claude Code")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    profile = agent_cli.detect_profile_for_pane("%138")

    assert profile is not None
    assert profile.name == "claude"


def test_detect_profile_for_pane_falls_back_to_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="100", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="200", command="codex", argv="codex"),
    ])

    profile = agent_cli.detect_profile_for_pane("%141")

    assert profile is not None
    assert profile.name == "codex"


def test_detect_profile_for_pane_finds_grok_on_the_tty(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "1.0.5")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="100", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(
            pid="200",
            command="grok",
            argv="grok --leader --leader-socket /home/.grok/hive/p19.sock",
        ),
    ])

    profile = agent_cli.detect_profile_for_pane("%19")

    assert profile is not None
    assert profile.name == "grok"


def test_detect_profile_for_pane_reads_codex_argv_without_claude_path_false_positive(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "node")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(
            pid="200",
            command="node",
            argv="node /opt/homebrew/bin/codex --cd /repo/.claude/worktrees/feature",
        ),
    ])

    profile = agent_cli.detect_profile_for_pane("%141")

    assert profile is not None
    assert profile.name == "codex"


def test_detect_profile_for_pane_claude_exe_not_misled_by_codex_title(monkeypatch):
    # Regression: macOS Claude Code reports comm "claude.exe". The command probe
    # must resolve it to claude so detection never falls back to the pane title,
    # which would misclassify a claude pane whose title mentions another CLI.
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude.exe")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "✳ Research Codex app server")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    profile = agent_cli.detect_profile_for_pane("%1")

    assert profile is not None
    assert profile.name == "claude"


def test_resolve_session_id_for_pane_dispatches_to_adapter(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    calls: list[str] = []

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            calls.append(pane_id)
            return "fake-sess"

    monkeypatch.setattr("hive.agent_cli.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert agent_cli.resolve_session_id_for_pane("%138") == "fake-sess"
    assert calls == ["%138"]


def test_resolve_session_id_for_pane_returns_pidfile_session(monkeypatch, tmp_path):
    """Adapter reads the PID-anchored pidfile to resolve the session."""
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-pid"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose"),
    ])

    assert agent_cli.resolve_session_id_for_pane("%138") == "sess-pid"


def test_resolve_session_id_for_pane_returns_none_when_no_profile(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.resolve_session_id_for_pane("%2") is None


def test_resolve_model_for_pane_reads_model_from_adapter_session(monkeypatch, tmp_path):
    """resolve_model_for_pane reads the model from whatever session the adapter
    resolves. For codex the daemon-first lookup lives inside the adapter, so the
    fake adapter here simply hands back a session id."""
    transcript = tmp_path / "rollout.jsonl"
    transcript.write_text("")

    class FakeMeta:
        model = "gpt-5.5"

    class FakeCodexAdapter:
        def resolve_current_session_id(self, _pane_id):
            return "sess-app"  # daemon-first resolution happens inside the adapter

        def find_session_file(self, session_id, *, cwd=None):
            return transcript if session_id == "sess-app" else None

        def read_meta(self, _path):
            return FakeMeta()

    monkeypatch.setattr(
        "hive.agent_cli.adapters.get",
        lambda name: FakeCodexAdapter() if name == "codex" else None,
    )
    monkeypatch.setattr("hive.agent_cli.tmux.display_value", lambda *_a, **_kw: "/work")

    assert agent_cli.resolve_model_for_pane("%1", cli_name="codex") == "gpt-5.5"


def test_resolve_model_for_pane_no_session_returns_current(monkeypatch):
    """When the adapter resolves no session (e.g. embedded codex with nothing
    open), resolve_model_for_pane keeps the caller's default."""

    class FakeCodexAdapter:
        def resolve_current_session_id(self, _pane_id):
            return None

    monkeypatch.setattr(
        "hive.agent_cli.adapters.get",
        lambda name: FakeCodexAdapter() if name == "codex" else None,
    )

    assert agent_cli.resolve_model_for_pane("%9", cli_name="codex", current_model="") == ""


def test_member_role_for_pane_returns_agent_when_profile_detected(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "codex")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%1") == "agent"


def test_member_role_for_pane_returns_terminal_for_shell(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%2") == "terminal"


# --- strict process matcher: argument text is never CLI identity ---


def test_process_matcher_rejects_cli_names_in_arguments():
    from hive.agent_cli import detect_profile_from_process

    # ordinary shell commands mentioning a CLI name must not read as a CLI
    assert detect_profile_from_process("rg", "rg codex src tests") is None
    assert detect_profile_from_process("git", "git grep claude") is None
    assert detect_profile_from_process("python", "python script.py codex") is None
    # a non-runtime argv[0] with a CLI-named script arg is not the wrapper shape
    assert detect_profile_from_process("node", "node script.js codex") is None


def test_process_matcher_accepts_executable_and_node_wrapper():
    from hive.agent_cli import detect_profile_from_process

    assert detect_profile_from_process("claude", "claude --verbose").name == "claude"
    assert detect_profile_from_process("claude.exe", "").name == "claude"
    assert detect_profile_from_process(
        "node", "node /opt/homebrew/bin/codex --remote unix:///s"
    ).name == "codex"
    # argv[0] identity works even when ps comm is generic
    assert detect_profile_from_process("something", "/usr/local/bin/claude --continue").name == "claude"
    assert detect_profile_from_process("grok", "grok --leader-socket /s").name == "grok"
    assert detect_profile_from_process("1.0.5", "/opt/homebrew/bin/grok --resume s").name == "grok"
    # a grok mention in argument text is still not a CLI
    assert detect_profile_from_process("rg", "rg grok src") is None


def test_claude_pid_for_pane_returns_the_claude_process_pid(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="123", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="456", command="claude", argv="claude --model x"),
    ])
    assert agent_cli.claude_pid_for_pane("%1") == 456


def test_claude_pid_for_pane_ignores_non_claude_processes(monkeypatch):
    # argv mentions of "claude" (rg, git grep) must not bind a pid: the same
    # process-identity rule the retained-shell probe uses
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="123", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="9", command="rg", argv="rg claude src"),
    ])
    assert agent_cli.claude_pid_for_pane("%1") is None
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    assert agent_cli.claude_pid_for_pane("%1") is None


def test_classify_model_family_reads_grok_models_as_xai():
    assert agent_cli.classify_model_family("grok-4.6") == "xai"
    assert agent_cli.classify_model_family("grok-build") == "xai"
    assert agent_cli.classify_model_family("claude-opus-4-8") == "anthropic"
    assert agent_cli.classify_model_family("gpt-5.5") == "openai"


# --- spawn model validation --------------------------------------------------


def _codex_cache(tmp_path, slugs):
    home = tmp_path / "codex"
    (home / "models_cache.json").parent.mkdir(parents=True, exist_ok=True)
    (home / "models_cache.json").write_text(
        json.dumps({"models": [{"slug": s} for s in slugs]})
    )
    return home


def _grok_cache(tmp_path, ids):
    home = tmp_path / "grok"
    home.mkdir(parents=True, exist_ok=True)
    (home / "models_cache.json").write_text(
        json.dumps({"models": {i: {} for i in ids}})
    )
    return home


def test_validate_spawn_model_accepts_a_catalog_hit(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(_codex_cache(tmp_path, ["gpt-5.6-sol", "gpt-5.5"])))
    assert agent_cli.validate_spawn_model("codex", "gpt-5.6-sol") is None


def test_validate_spawn_model_rejects_a_miss_with_a_hint(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(_codex_cache(tmp_path, ["gpt-5.6-sol", "gpt-5.5"])))
    error = agent_cli.validate_spawn_model("codex", "gpt-5.6-sole")
    assert error is not None
    assert "gpt-5.6-sole" in error and "gpt-5.6-sol" in error


def test_validate_spawn_model_reads_the_grok_catalog(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(_grok_cache(tmp_path, ["grok-4.6", "grok-4.5"])))
    assert agent_cli.validate_spawn_model("grok", "grok-4.6") is None
    assert agent_cli.validate_spawn_model("grok", "grok-build") is not None


def test_validate_spawn_model_fails_open_without_a_catalog(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "empty"))
    monkeypatch.setenv("GROK_HOME", str(tmp_path / "empty"))
    assert agent_cli.validate_spawn_model("codex", "gpt-anything") is None
    assert agent_cli.validate_spawn_model("grok", "grok-anything") is None
    # claude keeps no local catalog: always the CLI's own call
    assert agent_cli.validate_spawn_model("claude", "claude-nope") is None


def test_validate_spawn_model_ignores_empty_model():
    assert agent_cli.validate_spawn_model("codex", "") is None


def test_validate_spawn_model_refuses_cross_family_mistakes(tmp_path, monkeypatch):
    # no catalog needed: a gpt model on claude is wrong whatever the catalog says
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "none"))
    monkeypatch.setenv("GROK_HOME", str(tmp_path / "none"))
    assert agent_cli.validate_spawn_model("claude", "gpt-5.5") is not None
    assert agent_cli.validate_spawn_model("claude", "grok-4.6") is not None
    assert agent_cli.validate_spawn_model("codex", "claude-opus-5") is not None
    assert agent_cli.validate_spawn_model("grok", "gpt-5.6-sol") is not None
    # same family without a catalog still fails open
    assert agent_cli.validate_spawn_model("claude", "claude-opus-5") is None
