"""Tests for Agent.spawn model/skill/env handling."""

import json

import pytest

from hive import agent as agent_mod
from hive.adapters import claude_sessions
from hive.agent import (
    DeliveryError,
    Agent,
    detect_current_session_id,
)

# The real startup driver, captured before any monkeypatching, for tests that
# exercise its screen-capture loop instead of the fixture's success stub.
_REAL_STARTUP_DRIVER = agent_mod._drive_claude_startup


def _fake_session(pid: int = 4321, name: str = "swift-otter") -> claude_sessions.ClaudeSession:
    """A registry entry as claude_sessions.session_for_pid would return it."""
    return claude_sessions.ClaudeSession(
        name=name,
        pid=pid,
        cwd="/tmp",
        kind="cli",
        socket_path=f"/tmp/hive-test-inbox-{pid}.sock",
        session_id="sess-registry",
    )


def _pin_inbox(monkeypatch, *, pid, session):
    """Pin the claude delivery lookup chain: pane -> claude pid -> registry
    entry. Both halves inspect the live machine, so every test that reaches
    them must pin them. Returns the pids handed to session_for_pid."""
    seen_pids: list[int | None] = []
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _pane: pid)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_for_pid",
        lambda p: seen_pids.append(p) or session,
    )
    return seen_pids


def _pin_cli_probe(monkeypatch, name):
    """Pin the send gate's process probe (the real one inspects live tmux)."""
    from hive.agent_cli import get_profile

    profile = get_profile(name) if name else None
    monkeypatch.setattr(
        "hive.agent_cli.detect_cli_process_for_pane", lambda _pane: profile
    )


def _setup_tmux_mocks(monkeypatch):
    calls: list[str] = []
    tags: list[tuple[object, ...]] = []

    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.agent.tmux.split_window", lambda target, horizontal=True, size=None, cwd=None: target)
    monkeypatch.setattr("hive.agent.tmux.get_pane_tty", lambda _pane: None)
    monkeypatch.setattr("hive.agent.tmux.set_pane_title", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.tag_pane", lambda *args, **_kwargs: tags.append(args))
    monkeypatch.setattr("hive.agent.tmux.wait_for_text", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.wait_for_texts", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.is_pane_in_mode", lambda _pane: False)
    monkeypatch.setattr("hive.agent.tmux.cancel_pane_mode", lambda _pane: None)
    def _send_keys(_pane, text, enter=True):
        calls.append(text)
        if enter:
            calls.append("<Enter>")
    monkeypatch.setattr("hive.agent.tmux.send_keys", _send_keys)
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda _pane, key: calls.append(f"<{key}>"))
    monkeypatch.setattr("hive.agent.draft_guard.supported_profile", lambda _profile: False)
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    # Default: no shared codex daemon / grok leader, so tests never attempt a
    # real socket bind or spawn a CLI process. Tests that exercise the daemon
    # paths override these explicitly.
    monkeypatch.setattr("hive.adapters.codex_app_server.spawn_daemon", lambda *_a, **_kw: False)
    monkeypatch.setattr("hive.adapters.grok_leader.spawn_daemon", lambda *_a, **_kw: False)
    # Default: codex readiness (a live TUI process on the pane) reports success
    # so tests never poll the real process table for 30s.
    monkeypatch.setattr("hive.agent._wait_codex_attached", lambda *_a, **_kw: True)
    # Default: the startup driver reports the pane's claude session bound its
    # cross-session inbox, so spawn tests never run the capture loop or look
    # for a registry entry on disk. Failure paths have dedicated tests below.
    monkeypatch.setattr("hive.agent._drive_claude_startup", lambda _pane, _ready: True)

    return calls, tags


def test_spawn_rejects_outside_tmux(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: False)

    try:
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none")
    except ValueError as exc:
        assert "requires tmux" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_spawn_loads_specified_skill(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="demo-review",
    )

    assert "/demo-review" in calls[0]
    # Should NOT send hive bootstrap message
    assert not any("hive teammate" in c for c in calls)


def test_spawn_skips_skill_when_none(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    assert not any(c.startswith("/") and not c.startswith("/tmp") for c in calls)


def test_spawn_passes_extra_env(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
        extra_env={"CR_WORKSPACE": "/tmp/cr-test"},
    )

    startup_cmd = calls[0]
    assert "CR_WORKSPACE=" in startup_cmd
    assert "/tmp/cr-test" in startup_cmd
    assert "HIVE_TEAM_NAME=" not in startup_cmd
    assert "HIVE_AGENT_NAME=" not in startup_cmd


def test_spawn_without_extra_env_does_not_export_default_hive_vars(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    startup_cmd = calls[0]
    assert "HIVE_TEAM_NAME=" not in startup_cmd
    assert "HIVE_AGENT_NAME=" not in startup_cmd
    assert "export " not in startup_cmd


def test_spawn_hive_loads_skill_and_sends_prompt(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.",
    )

    # Skill activation + user prompt are passed as the [prompt] positional arg.
    startup_cmd = calls[0]
    assert "/hive" in startup_cmd
    assert "Please check your inbox." in startup_cmd


def test_spawn_codex_hive_loads_skill_and_sends_prompt(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.", cli="codex",
    )

    # Skill activation + user prompt are passed as the [PROMPT] positional
    # arg (avoids TUI keystroke race against the codex skill picker).
    startup_cmd = calls[0]
    assert "$hive" in startup_cmd
    assert "Please check your inbox." in startup_cmd
    # Only the initial `cd ... && exec codex` Enter — no follow-up TUI inject.
    assert calls.count("<Enter>") == 1


def test_spawn_claude_ready_as_soon_as_pid_and_registry_resolve(monkeypatch):
    # exercise the real startup driver: readiness is the pane's claude pid
    # resolving to a registry entry, so an empty screen (no welcome banner —
    # what a resumed session shows) is enough
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent._drive_claude_startup", _REAL_STARTUP_DRIVER)
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.agent._PROMPT_SETTLE", 0.0)
    seen = _pin_inbox(monkeypatch, pid=4321, session=_fake_session())

    agent = Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                        is_first=True, cli="claude")

    assert agent.pane_id == "%0"
    assert seen == [4321]  # one probe round, no banner ever consulted
    assert calls[0].split(" && ")[-1].split()[:2] == ["hive", "claude"]


def test_startup_driver_true_when_inbox_registers(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.agent._PROMPT_SETTLE", 0.0)
    _pin_inbox(monkeypatch, pid=4321, session=_fake_session())

    assert agent_mod._drive_claude_startup("%9", "Claude Code") is True


def test_startup_driver_false_when_ready_without_inbox(monkeypatch):
    # the TUI is up but no session registration ever appears: after the grace
    # window the driver gives up instead of waiting out the whole timeout
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "Claude Code\n")
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.agent.AGENT_STARTUP_TIMEOUT", 30)
    monkeypatch.setattr("hive.agent._INBOX_NOTICE_GRACE", 0.01)
    _pin_inbox(monkeypatch, pid=4321, session=None)

    assert agent_mod._drive_claude_startup("%9", "Claude Code") is False


def test_startup_driver_false_when_no_claude_pid_before_deadline(monkeypatch):
    # no claude process on the pane tty at all: nothing to register an inbox
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.agent.AGENT_STARTUP_TIMEOUT", 0)
    probed: list[int | None] = _pin_inbox(monkeypatch, pid=None, session=_fake_session())

    assert agent_mod._drive_claude_startup("%9", "Claude Code") is False
    assert probed == []  # a missing pid short-circuits the registry read


def test_startup_driver_answers_startup_prompts_until_inbox_appears(monkeypatch):
    # folder trust / MCP consent each get one Enter (the safe first option);
    # a prompt on screen always outranks the inbox probe
    screens = ["Do you trust this folder?\n", "New MCP server found\n"]
    monkeypatch.setattr(
        "hive.agent.tmux.capture_pane",
        lambda *_a, **_kw: screens.pop(0) if screens else "Claude Code\n",
    )
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    keys: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: keys.append((pane, key)))
    probes: list[int] = []
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _pane: 4321)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_for_pid",
        lambda p: probes.append(p) or _fake_session(),
    )

    assert agent_mod._drive_claude_startup("%9", "Claude Code") is True
    assert keys == [("%9", "Enter"), ("%9", "Enter")]
    assert probes == [4321]  # never probed while a dialog was on screen


def test_startup_driver_answers_prompts_even_after_inbox_registers(monkeypatch):
    # the inbox registers at process start, BEFORE the trust dialog can render:
    # registration alone must not end the drive with a modal still blocking
    screens = ["", "Do you trust this folder?\n"]
    monkeypatch.setattr(
        "hive.agent.tmux.capture_pane",
        lambda *_a, **_kw: screens.pop(0) if screens else "Claude Code\n",
    )
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    keys: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: keys.append((pane, key)))
    _pin_inbox(monkeypatch, pid=4321, session=_fake_session())

    assert agent_mod._drive_claude_startup("%9", "Claude Code") is True
    assert keys == [("%9", "Enter")]  # the late dialog still got answered


@pytest.mark.parametrize("cli_name", ["claude", "codex", "grok"])
def test_spawn_rejects_prompt_starting_with_dash(monkeypatch, cli_name):
    # the launch goes through `hive <cli>`, whose parser strips any `--`
    # separator, so a dashed prompt would be read as a flag: refuse it
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    _mock_grok_leader_up(monkeypatch)

    with pytest.raises(ValueError, match="must not start with '-'"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli=cli_name, skill="none", prompt="--edge prompt")


@pytest.mark.parametrize("cli_name", ["claude", "codex", "grok"])
def test_spawn_pane_command_runs_hive_launcher_then_resume_hint(monkeypatch, cli_name):
    # the pane runs hive's managed launcher as the binary (never the rc's
    # hclaude/hcodex/hgrok function) and prints the cd-ready hint once the CLI
    # exits
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    _mock_grok_leader_up(monkeypatch)

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/work/dir",
                is_first=True, cli=cli_name, skill="none")

    launch = calls[0].split(" && ")[-1]
    tail = f"; hive resume-hint {cli_name} 2>/dev/null || true"
    assert launch.endswith(tail)
    # token check, not a prefix: a bare claude launch now carries no flags
    assert launch[: -len(tail)].split()[:2] == ["hive", cli_name]


def test_spawn_claude_resume_proves_the_inbox_too(monkeypatch):
    # a resumed session is a new claude process: it must register its own
    # inbox before spawn hands the member over, exactly like a fresh spawn
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent._drive_claude_startup", _REAL_STARTUP_DRIVER)
    # a resumed session never renders the welcome banner
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.agent._PROMPT_SETTLE", 0.0)
    seen = _pin_inbox(monkeypatch, pid=777, session=_fake_session(pid=777))

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude", session_id="sess-123")

    assert seen == [777]  # the resumed process's own registry entry
    assert "sess-123" in calls[0]


def test_spawn_claude_without_inbox_kills_pane_and_fails(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent._drive_claude_startup", lambda _pane, _ready: False)
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)

    with pytest.raises(RuntimeError, match="inbox-only"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert killed == ["%0"]  # the half-started pane is not left behind


def test_spawn_tags_pane_before_waiting_for_ready(monkeypatch):
    calls, tags = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent.tmux.wait_for_texts", lambda *_args, **_kw: False)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%9",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
    )

    assert calls, "spawn should still start the CLI process"
    assert tags == [("%9", "agent", "w1", "t")]


def test_spawn_claude_uses_model_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="opus", cwd="/tmp", is_first=True,
        skill="none", cli="claude",
    )

    startup_cmd = calls[0]
    assert "--model 'opus'" in startup_cmd
    assert "claude" in startup_cmd


def test_spawn_codex_pins_model_at_mint_not_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_daemon_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="gpt-5.2", cwd="/tmp", is_first=True,
        skill="none", cli="codex",
    )

    startup_cmd = calls[0]
    # model is a thread/start property, not a resume flag
    assert "-m 'gpt-5.2'" not in startup_cmd
    assert state["minted"] == [("/tmp", "t.w1", "gpt-5.2")]


def test_spawn_rejects_unknown_cli(monkeypatch):
    _setup_tmux_mocks(monkeypatch)

    try:
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none", cli="vim")
    except ValueError as exc:
        assert "unsupported cli" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_spawn_claude_resume_uses_fork_session(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "-r 'sess-abc'" in startup_cmd
    assert "--fork-session" in startup_cmd


def test_spawn_codex_resume_uses_fork_subcommand(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="codex",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert startup_cmd.split(" && ")[-1].startswith(
        "hive codex -c check_for_update_on_startup=false fork 'sess-abc'")
    # codex fork does not take --model; model flag should not appear
    assert "-m" not in startup_cmd


def test_spawn_codex_new_session_resumes_minted_thread(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_daemon_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
    )

    startup_cmd = calls[0]
    # hive minted the thread, recorded the pane binding, trusted the cwd, and
    # the pane attaches with `resume <tid>` — the managed launcher injects
    # --remote/--cd itself, so the spawn command carries neither.
    assert "resume 'tid-minted'" in startup_cmd
    assert "--remote" not in startup_cmd
    assert "--cd" not in startup_cmd
    assert state["minted"] == [("/work/dir", "t.w1", "")]
    assert state["trusted"] == ["/work/dir"]
    assert state["records"] == [("%0", "tid-minted", "/work/dir")]


def test_spawn_codex_mint_failure_kills_pane_and_fails(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.start_member_thread",
        lambda *_a, **_kw: None,
    )
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)

    with pytest.raises(RuntimeError, match="thread identity"):
        Agent.spawn(
            name="w1", team_name="t", target_pane="%0",
            cwd="/work/dir", is_first=True, skill="none", cli="codex",
        )

    assert killed == ["%0"]
    assert calls == []  # no startup command was ever sent


def _mock_daemon_up(monkeypatch):
    """Shared daemon up; record trust writes, thread mints and pane records."""
    state = {"minted": [], "trusted": [], "records": []}
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.spawn_daemon", lambda *_a, **_kw: True
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.ensure_dir_trusted",
        lambda cwd: state["trusted"].append(cwd),
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.start_member_thread",
        lambda cwd, *, name, model="": (
            state["minted"].append((cwd, name, model)) or "tid-minted"
        ),
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.write_pane_thread",
        lambda pane, tid, cwd: state["records"].append((pane, tid, cwd)),
    )
    return state


def _mock_grok_leader_up(monkeypatch):
    """Leader daemon up; record the session hive minted for the pane."""
    sessions: list[tuple[str, str, str]] = []
    monkeypatch.setattr("hive.adapters.grok_leader.spawn_daemon", lambda *_a, **_kw: True)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.write_pane_session",
        lambda pane, session_id, cwd: sessions.append((pane, session_id, cwd)),
    )
    # Readiness polls the minted session dir on disk; answer immediately.
    monkeypatch.setattr("hive.agent._wait_grok_session_ready", lambda _pane, _sid: True)
    return sessions


def test_spawn_codex_preconnects_2nd_client_with_workspace(monkeypatch):
    # With a workspace, spawn asks the sidecar to bring its client online
    # before the member's first turn.
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    connects: list[str] = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda workspace: connects.append(workspace) or {"ok": True},
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
        workspace="/tmp/ws",
    )

    assert connects == ["/tmp/ws"]


def test_spawn_codex_skips_preconnect_without_workspace(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    connects: list = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda workspace: connects.append(workspace),
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
    )  # no workspace → no eager preconnect, lazy tick covers it

    assert connects == []


def test_spawn_codex_new_session_refuses_when_daemon_fails(monkeypatch):
    """Embedded codex is unsupported: if the per-pane daemon cannot bind, spawn
    must not launch a raw codex as a team member — it kills the pane it just
    split and raises instead of leaving a stateless tagged member behind."""
    # _setup_tmux_mocks makes spawn_daemon return False (daemon failed to bind).
    calls, _ = _setup_tmux_mocks(monkeypatch)
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", lambda pane: killed.append(pane))

    with pytest.raises(RuntimeError, match="daemon-only"):
        Agent.spawn(
            name="w1", team_name="t", target_pane="%0",
            cwd="/work/dir", is_first=True, skill="none", cli="codex",
        )

    assert killed == ["%0"]  # the split pane is cleaned up
    assert calls == []  # no startup command was ever sent


def test_spawn_codex_daemon_fail_in_place_clears_tags_instead_of_killing(monkeypatch):
    """split_window=False spawns into the caller's own shell pane: on daemon
    failure that pane must survive, but the hive tags just written are undone."""
    calls, _ = _setup_tmux_mocks(monkeypatch)
    killed: list[str] = []
    cleared: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", lambda pane: killed.append(pane))
    monkeypatch.setattr("hive.agent.tmux.clear_pane_tags", lambda pane: cleared.append(pane))

    with pytest.raises(RuntimeError, match="daemon-only"):
        Agent.spawn(
            name="w1", team_name="t", target_pane="%0",
            cwd="/work/dir", is_first=True, skill="none", cli="codex",
            split_window=False,
        )

    assert killed == []
    assert cleared == ["%0"]
    assert calls == []


def test_spawn_codex_fork_does_not_start_daemon(monkeypatch):
    # The pane's `hive codex fork <sid>` binds the daemon, forks server-side
    # and records the pane's thread itself; spawn stays out of it.
    calls, _ = _setup_tmux_mocks(monkeypatch)
    started: list[object] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.spawn_daemon",
        lambda *a, **k: started.append(a) or True,
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "fork" in startup_cmd and "sess-abc" in startup_cmd
    assert "--remote" not in startup_cmd  # the launcher injects it
    assert started == []  # daemon not started by spawn for a fork


def test_spawn_grok_launches_with_minted_session_id_and_model_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    sessions = _mock_grok_leader_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="grok-4.6", cwd="/work/dir", is_first=True, skill="none", cli="grok",
    )

    launch = calls[0].split(" && ")[-1].split("; hive resume-hint")[0]
    pane, session_id, cwd = sessions[0]
    assert (pane, cwd) == ("%0", "/work/dir")
    assert launch.split() == ["hive", "grok", "--session-id", session_id, "-m", "'grok-4.6'"]


def test_spawn_grok_resume_keeps_the_session_id_and_drops_fork_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    sessions = _mock_grok_leader_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none",
        cli="grok", session_id="sess-abc", session_mode="resume",
    )

    launch = calls[0].split(" && ")[-1].split("; hive resume-hint")[0]
    assert launch.split() == ["hive", "grok", "--resume", "'sess-abc'"]
    # the pane drives the resumed session itself — no new id is minted
    assert sessions == [("%0", "sess-abc", "/tmp")]


def test_spawn_grok_fork_mints_a_new_session_id_for_the_branch(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    sessions = _mock_grok_leader_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none",
        cli="grok", session_id="sess-abc",
    )

    launch = calls[0].split(" && ")[-1].split("; hive resume-hint")[0]
    forked_id = sessions[0][1]
    assert forked_id != "sess-abc"
    assert launch.split() == [
        "hive", "grok", "--session-id", forked_id, "--resume", "'sess-abc'", "--fork-session",
    ]


def test_spawn_grok_refuses_when_leader_daemon_fails(monkeypatch):
    """Grok runtime lives on the per-pane leader: without one the pane would run
    a grok nobody can reach, so spawn gives the pane back and raises."""
    # _setup_tmux_mocks makes grok spawn_daemon return False.
    calls, _ = _setup_tmux_mocks(monkeypatch)
    killed: list[str] = []
    written: list[tuple] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.write_pane_session",
        lambda *a: written.append(a),
    )

    with pytest.raises(RuntimeError, match="leader-only"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    skill="none", cli="grok")

    assert killed == ["%0"]
    assert calls == []  # no launch command was ever sent
    assert written == []  # and no session record left behind


def test_spawn_grok_leader_fail_in_place_clears_tags_instead_of_killing(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    killed: list[str] = []
    cleared: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)
    monkeypatch.setattr("hive.agent.tmux.clear_pane_tags", cleared.append)

    with pytest.raises(RuntimeError, match="leader-only"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    skill="none", cli="grok", split_window=False)

    assert killed == []
    assert cleared == ["%0"]
    assert calls == []


def test_spawn_grok_connects_the_2nd_client_once_the_session_is_ready(monkeypatch):
    # the client can only load a session the TUI has opened, so the connect
    # follows readiness instead of racing the launch
    _setup_tmux_mocks(monkeypatch)
    _mock_grok_leader_up(monkeypatch)
    order: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._wait_grok_session_ready",
        lambda pane, _sid: order.append(("ready", pane)) or True,
    )
    monkeypatch.setattr(
        "hive.sidecar.request_connect_grok",
        lambda workspace, pane: order.append(("connect", workspace, pane)) or {"ok": True},
    )

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/work/dir",
                skill="none", cli="grok", workspace="/tmp/ws")

    assert order == [("ready", "%0"), ("connect", "/tmp/ws", "%0")]


def test_spawn_grok_skips_the_connect_when_readiness_times_out(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_grok_leader_up(monkeypatch)
    monkeypatch.setattr("hive.agent._wait_grok_session_ready", lambda _pane, _sid: False)
    connects: list = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_grok",
        lambda workspace, pane: connects.append((workspace, pane)),
    )

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/work/dir",
                skill="none", cli="grok", workspace="/tmp/ws")

    assert connects == []  # nothing to load yet; the lazy connect retries


def test_spawn_grok_skips_preconnect_without_workspace(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_grok_leader_up(monkeypatch)
    connects: list = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_grok",
        lambda workspace, pane: connects.append((workspace, pane)),
    )

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/work/dir",
                skill="none", cli="grok")  # lazy connect on the next tick covers it

    assert connects == []


def test_send_codex_uses_turn_start_when_daemon_accepts(monkeypatch):
    # pin the process probe: the real one inspects the live tmux pane "%3",
    # which detects whatever CLI happens to run there on this machine
    _pin_cli_probe(monkeypatch, "codex")
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda pane, text: sent.append((pane, text)) or True,
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="codex").send("hi")

    assert sent == [("%3", "hi")]
    assert submitted == []  # no keystroke fallback when daemon accepts


def test_send_uses_detected_codex_daemon_when_stored_cli_is_stale(monkeypatch):
    sent: list[tuple[str, str]] = []
    _pin_cli_probe(monkeypatch, "codex")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda pane, text: sent.append((pane, text)) or True,
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert sent == [("%3", "hi")]
    assert submitted == []


def test_send_codex_accepted_returns_classification_without_keystrokes(monkeypatch):
    # pin the process probe: the real one inspects the live tmux pane "%3",
    # which detects whatever CLI happens to run there on this machine
    _pin_cli_probe(monkeypatch, "codex")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda pane, text: "turnStartAccepted",
    )

    accepted = Agent(name="w", team_name="t", pane_id="%3", cli="codex").send("hi")

    assert accepted == "turnStartAccepted"
    assert calls == []  # native transport only — the composer is never touched


def test_send_codex_transport_failure_raises_without_keystrokes(monkeypatch):
    """VAL-5: any codex transport failure (no daemon, no thread, RPC error,
    exception — the adapter folds them all to None) raises DeliveryError and
    never falls back to keystroke injection."""
    _pin_cli_probe(monkeypatch, "codex")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane", lambda pane, text: None
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    with pytest.raises(DeliveryError):
        Agent(name="w", team_name="t", pane_id="%3", cli="codex").send("hi")

    assert submitted == []
    assert calls == []


def test_send_grok_queues_the_prompt_on_the_leader(monkeypatch):
    _pin_cli_probe(monkeypatch, "grok")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.send_to_pane",
        lambda pane, text: sent.append((pane, text)) or "sessionPromptQueued",
    )

    accepted = Agent(name="w", team_name="t", pane_id="%3", cli="grok").send("hi")

    assert accepted == "sessionPromptQueued"
    assert sent == [("%3", "hi")]
    assert calls == []  # native transport only — the composer is never touched


def test_send_grok_transport_failure_raises_without_keystrokes(monkeypatch):
    """Every grok transport failure (no leader, no session record, RPC error,
    ack timeout — the adapter folds them all to None) raises DeliveryError and
    never falls back to keystroke injection."""
    _pin_cli_probe(monkeypatch, "grok")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.send_to_pane", lambda pane, text: None
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    with pytest.raises(DeliveryError):
        Agent(name="w", team_name="t", pane_id="%3", cli="grok").send("hi")

    assert submitted == []
    assert calls == []


def test_send_claude_writes_to_the_session_inbox_as_the_member_address(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    session = _fake_session()
    _pin_inbox(monkeypatch, pid=session.pid, session=session)
    writes: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: writes.append((sock, text, sender))
        or claude_sessions.ACCEPTED_UDS_WRITE,
    )

    accepted = Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert accepted == "udsWriteAccepted"
    assert writes == [(session.socket_path, "hi", "t.w")]
    assert calls == []  # native transport only — the composer is never touched


def test_send_claude_resolves_the_inbox_from_the_pane_claude_pid(monkeypatch):
    # the delivery address is derived pane -> live claude pid -> registry
    # entry; a pid from another pane must never be what gets messaged
    _pin_cli_probe(monkeypatch, "claude")
    _setup_tmux_mocks(monkeypatch)
    panes: list[str] = []
    monkeypatch.setattr(
        "hive.agent_cli.claude_pid_for_pane",
        lambda pane: panes.append(pane) or 9182,
    )
    seen_pids: list[int] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_for_pid",
        lambda pid: seen_pids.append(pid) or _fake_session(pid=pid),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: claude_sessions.ACCEPTED_UDS_WRITE,
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert panes == ["%3"]
    assert seen_pids == [9182]  # the pane's own claude pid keys the registry


def test_send_claude_without_registered_inbox_raises(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _pin_inbox(monkeypatch, pid=4321, session=None)
    writes: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda *a, **kw: writes.append((a, kw)) or claude_sessions.ACCEPTED_UDS_WRITE,
    )

    with pytest.raises(DeliveryError, match="inbox-only"):
        Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert writes == []  # no socket to write to; nothing was attempted
    assert calls == []


def test_send_claude_not_listening_raises_without_keystrokes(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _pin_inbox(monkeypatch, pid=4321, session=_fake_session())
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send", lambda *a, **kw: None
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    with pytest.raises(DeliveryError, match="not listening"):
        Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert submitted == []
    assert calls == []


def test_send_claude_write_timeout_raises_and_is_not_an_accept(monkeypatch):
    # the listener took the connection but never read the frame: a stalled
    # session, reported as a failure rather than returned as a classification
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _pin_inbox(monkeypatch, pid=4321, session=_fake_session())
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda *a, **kw: claude_sessions.WRITE_TIMED_OUT,
    )

    with pytest.raises(DeliveryError, match="did not drain the message in time"):
        Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert calls == []


def test_send_unknown_profile_raises_without_keystrokes(monkeypatch):
    # no CLI process on the pane TTY: the send gate refuses before any transport
    _pin_cli_probe(monkeypatch, "")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    with pytest.raises(DeliveryError):
        Agent(name="w", team_name="t", pane_id="%3", cli="mystery").send("hi")

    assert submitted == []
    assert calls == []


def test_send_claude_never_uses_codex_daemon(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    _pin_inbox(monkeypatch, pid=4321, session=_fake_session())
    daemon_calls: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda *a: daemon_calls.append(a) or True,
    )
    inbox_calls: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: inbox_calls.append((sock, text, sender))
        or claude_sessions.ACCEPTED_UDS_WRITE,
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert daemon_calls == []  # codex daemon path not taken for claude
    assert len(inbox_calls) == 1  # claude delivers over its session inbox
    assert submitted == []  # inbox-only: no keystroke fallback


def test_spawn_claude_skips_session_detection(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    resolved: list[str] = []
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda pane_id: resolved.append(pane_id) or None)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
    )

    assert resolved == [], "should not resolve session for claude"


def test_detect_current_session_id_delegates_to_resolve(monkeypatch):
    monkeypatch.setattr(
        "hive.agent.resolve_session_id_for_pane",
        lambda pane_id: "map-sess-1" if pane_id == "%11" else None,
    )

    assert detect_current_session_id("/tmp/test", pane_id="%11") == "map-sess-1"
    assert detect_current_session_id("/tmp/test", pane_id="%99") is None


# --- session_mode: fork vs resume (VAL B5-B7) ---


def test_spawn_claude_fork_and_resume_session_flags(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="sess-1")
    fork_cmd = calls[-2]
    assert "-r 'sess-1'" in fork_cmd or "-r sess-1" in fork_cmd
    assert "--fork-session" in fork_cmd

    calls.clear()
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="sess-1", session_mode="resume")
    resume_cmd = calls[-2]
    assert "-r 'sess-1'" in resume_cmd or "-r sess-1" in resume_cmd
    assert "--fork-session" not in resume_cmd


def test_spawn_codex_fork_delegates_to_hive_codex(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    # spawn itself never touches the daemon for a fork (the pane's `hive codex`
    # binds it); the default spawn_daemon mock returning False must not matter.
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", session_id="roll-1")

    launch = calls[0].split(" && ")[-1].split("; hive resume-hint")[0]
    assert launch.startswith("hive codex ")
    assert "fork 'roll-1'" in launch
    assert "--remote" not in launch  # the daemon binding is `hive codex`'s job
    assert "resume" not in launch


def test_spawn_codex_resume_records_thread_and_resumes_it(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_daemon_up(monkeypatch)
    connects: list[str] = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda ws: connects.append(ws),
    )

    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/repo",
                cli="codex", session_id="roll-1", session_mode="resume",
                skill="none", workspace="/ws")

    cmd = calls[0]
    # the resumed session's id IS its threadId: recorded, then resumed through
    # the managed launcher (which injects --remote/--cd itself)
    assert "resume 'roll-1'" in cmd
    assert "fork" not in cmd
    assert "--remote" not in cmd
    assert state["minted"] == []  # nothing minted on resume
    assert state["records"] == [("%0", "roll-1", "/repo")]
    assert connects == ["/ws"]


def test_spawn_codex_resume_daemon_failure_never_falls_back_embedded(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", lambda pane: killed.append(pane))

    # split path: new pane is killed
    with pytest.raises(RuntimeError, match="daemon"):
        Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                    cli="codex", session_id="roll-1", session_mode="resume")
    assert killed == ["%0"]
    assert calls == []  # no command was ever typed — no embedded fallback

    # in-place path: tags/title cleared instead
    cleared: list[str] = []
    titles: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.agent.tmux.clear_pane_tags", lambda pane: cleared.append(pane))
    monkeypatch.setattr("hive.agent.tmux.set_pane_title", lambda pane, title: titles.append((pane, title)))
    with pytest.raises(RuntimeError, match="daemon"):
        Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                    cli="codex", session_id="roll-1", session_mode="resume",
                    split_window=False)
    assert cleared == ["%0"]
    assert ("%0", "") in titles
    assert calls == []


def test_spawn_rejects_unknown_session_mode(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    with pytest.raises(ValueError, match="session_mode"):
        Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                    cli="claude", session_id="s", session_mode="clone")


# --- readiness oracles: runtime signals, not screen text (VAL 1-7) ---


def _watch_banner_and_sleep(monkeypatch):
    banner_waits: list[tuple] = []
    sleeps: list[float] = []
    monkeypatch.setattr(
        "hive.agent.tmux.wait_for_text",
        lambda pane, text, timeout=0: banner_waits.append((pane, text)) or True,
    )
    monkeypatch.setattr("hive.agent.time.sleep", lambda d: sleeps.append(d))
    return banner_waits, sleeps


def test_spawn_claude_inbox_readiness_skips_banner_and_settle(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    banner_waits, sleeps = _watch_banner_and_sleep(monkeypatch)

    # fresh and resume: the inbox registration is the oracle, the banner
    # (which a resumed session never renders) is not consulted at all
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp", cli="claude")
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="sess-1", session_mode="resume")

    assert banner_waits == []
    assert 1 not in sleeps  # no fixed 1s settle either


def test_spawn_codex_waits_on_process_not_banner(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    banner_waits, _ = _watch_banner_and_sleep(monkeypatch)
    waited: list[str] = []
    monkeypatch.setattr(
        "hive.agent._wait_codex_attached",
        lambda pane, **_kw: waited.append(pane) or True,
    )

    Agent.spawn(name="v", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", skill="none", session_id="roll-1", session_mode="resume")

    assert banner_waits == []
    assert waited == ["%0"]


def test_wait_codex_attached_polls_for_the_codex_process(monkeypatch):
    from hive.agent_cli import get_profile

    profiles = iter([None, get_profile("claude"), get_profile("codex")])
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr(
        "hive.agent_cli.detect_cli_process_for_pane", lambda _p: next(profiles)
    )
    # None and a non-codex profile are both "not attached yet"
    assert agent_mod._wait_codex_attached("%9", timeout=60, interval=0) is True


def test_wait_codex_attached_timeout_is_deterministic_and_nonfatal(monkeypatch):
    monkeypatch.setattr(
        "hive.agent_cli.detect_cli_process_for_pane", lambda _p: None
    )
    assert agent_mod._wait_codex_attached("%9", timeout=0, interval=0) is False

    # spawn survives a readiness timeout and still completes
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    monkeypatch.setattr("hive.agent._wait_codex_attached", lambda _p: False)

    a = Agent.spawn(name="v", team_name="t", target_pane="%0", cwd="/tmp",
                    cli="codex", skill="hive")
    assert a.pane_id == "%0"


def test_spawn_grok_waits_on_the_minted_session_dir_not_the_banner(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.adapters.grok_leader.spawn_daemon", lambda *_a, **_kw: True)
    sessions: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.write_pane_session",
        lambda pane, session_id, cwd: sessions.append((pane, session_id, cwd)),
    )
    banner_waits, _ = _watch_banner_and_sleep(monkeypatch)
    waited: list[str] = []
    monkeypatch.setattr(
        "hive.agent._wait_grok_session_ready", lambda pane, sid: waited.append(sid) or True
    )

    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="grok", skill="none")

    assert banner_waits == []
    assert waited == [sessions[0][1]]  # the id hive minted, not the pane's cwd


def test_wait_grok_session_ready_sees_the_session_dir_and_is_nonfatal(monkeypatch, tmp_path):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    _pin_cli_probe(monkeypatch, "grok")
    assert agent_mod._wait_grok_session_ready("%0", "sess-x", timeout=0, interval=0) is False

    # grok creates $GROK_HOME/sessions/<quoted cwd>/<sid>/ at startup
    (tmp_path / "sessions" / "%2Ftmp" / "sess-x").mkdir(parents=True)
    assert agent_mod._wait_grok_session_ready("%0", "sess-x", timeout=0, interval=0) is True

    # on resume the dir predates the launch, so the pane's own grok must be up
    _pin_cli_probe(monkeypatch, "")
    assert agent_mod._wait_grok_session_ready("%0", "sess-x", timeout=0, interval=0) is False

    # a readiness timeout is not fatal: spawn still completes
    _setup_tmux_mocks(monkeypatch)
    _mock_grok_leader_up(monkeypatch)
    monkeypatch.setattr("hive.agent._wait_grok_session_ready", lambda _pane, _sid: False)

    agent = Agent.spawn(name="v", team_name="t", target_pane="%0", cwd="/tmp",
                        cli="grok", skill="hive")
    assert agent.pane_id == "%0"


def test_spawn_codex_fork_waits_on_process_not_banner(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    banner_waits, _ = _watch_banner_and_sleep(monkeypatch)
    waited: list[str] = []
    monkeypatch.setattr(
        "hive.agent._wait_codex_attached",
        lambda pane, **_kw: waited.append(pane) or True,
    )

    Agent.spawn(name="f", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", session_id="roll-1")  # fork mode

    assert banner_waits == []  # every codex spawn shares the process oracle
    assert waited == ["%0"]


# --- V1: the launch never execs — the pane shell must survive the CLI ---


def _assert_launch_keeps_shell(startup_cmd: str) -> None:
    """The CLI must run as the shell's foreground child: no `exec` token may
    appear in the launch pipeline (quoted prompt text does not count as a
    token, so this cannot green on substrings)."""
    import shlex as _shlex

    for segment in startup_cmd.split("&&"):
        try:
            tokens = _shlex.split(segment)
        except ValueError:
            tokens = segment.split()
        assert "exec" not in tokens, startup_cmd


def test_launch_guard_catches_the_old_exec_form():
    # negative control: the pre-change launch shape must trip the assertion
    with pytest.raises(AssertionError):
        _assert_launch_keeps_shell("cd '/w' && exec /bin/codex --remote 'unix:///s'")


def test_spawn_claude_fresh_launch_keeps_shell(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none")
    _assert_launch_keeps_shell(calls[0])
    assert "claude" in calls[0]


def test_spawn_claude_resume_launch_keeps_shell(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    Agent.spawn(
        name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none",
        session_id="sess-1", session_mode="resume",
    )
    startup_cmd = calls[0]
    _assert_launch_keeps_shell(startup_cmd)
    assert "-r 'sess-1'" in startup_cmd  # resume flags unchanged


def test_spawn_codex_daemon_native_launch_keeps_shell(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
    )
    startup_cmd = calls[0]
    _assert_launch_keeps_shell(startup_cmd)
    assert "resume 'tid-minted'" in startup_cmd  # minted-thread attach shape


def test_spawn_codex_fork_shortcut_launch_keeps_shell(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    Agent.spawn(
        name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none",
        cli="codex", session_id="sess-abc",
    )
    startup_cmd = calls[0]
    _assert_launch_keeps_shell(startup_cmd)
    assert "fork" in startup_cmd and "sess-abc" in startup_cmd
