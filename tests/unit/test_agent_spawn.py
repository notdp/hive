"""Tests for Agent.spawn model/skill/env handling."""

import json

import pytest

from hive import agent as agent_mod
from hive.agent import (
    DeliveryError,
    Agent,
    detect_current_session_id,
)

# The real startup driver, captured before any monkeypatching, for tests that
# exercise its screen-capture loop instead of the fixture's success stub.
_REAL_CHANNEL_STARTUP = agent_mod._drive_claude_channel_startup

_CHANNEL_FLAGS = ["--channels", "plugin:hive-channel@hive"]


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
    # Default: no per-pane codex daemon, so tests never attempt a real socket
    # bind. Tests that exercise the --remote path override this explicitly.
    monkeypatch.setattr("hive.adapters.codex_app_server.spawn_daemon", lambda *_a, **_kw: False)
    # Default: claude channel registration succeeds without touching disk and
    # the startup driver reports the channel ready, so spawn tests never write
    # the channel config or drive the capture loop. Failure paths have
    # dedicated tests below and in tests/unit/test_claude_channel.py + tests/cli.
    monkeypatch.setattr(
        "hive.adapters.claude_channel.prepare_pane", lambda _cwd: list(_CHANNEL_FLAGS)
    )
    monkeypatch.setattr(
        "hive.agent._drive_claude_channel_startup", lambda _pane, _ready: True
    )

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


def test_spawn_claude_launches_with_channel_flags(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    # exercise the real startup driver: readiness comes from the marker the
    # channel server writes, not from any screen text
    monkeypatch.setattr("hive.agent._drive_claude_channel_startup", _REAL_CHANNEL_STARTUP)
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "Claude Code\n")
    monkeypatch.setattr("hive.adapters.claude_channel.is_ready", lambda _pane: True)

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude")

    startup_cmd = calls[0]
    assert "--channels" in startup_cmd
    assert "plugin:hive-channel@hive" in startup_cmd


def test_spawn_claude_clears_stale_marker_before_launch(monkeypatch):
    # a marker left by a previous claude on this pane id must not count as
    # the new server's readiness: spawn clears it, the driver times out, and
    # the spawn fails instead of silently accepting an undeliverable pane
    _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent._drive_claude_channel_startup", _REAL_CHANNEL_STARTUP)
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "Claude Code\n")
    monkeypatch.setattr("hive.agent.tmux.kill_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent.AGENT_STARTUP_TIMEOUT", 1)
    monkeypatch.setattr("hive.agent._CHANNEL_NOTICE_GRACE", 0.01)

    stale = {"%0"}  # simulated marker store: pre-seeded stale entry
    monkeypatch.setattr("hive.adapters.claude_channel.is_ready",
                        lambda pane: pane in stale)
    monkeypatch.setattr("hive.adapters.claude_channel.clear_ready", stale.discard)

    with pytest.raises(RuntimeError, match="channel"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert stale == set()  # the stale marker was cleared, never trusted


def test_channel_startup_driver_true_when_marker_appears(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.adapters.claude_channel.is_ready", lambda _pane: True)

    assert agent_mod._drive_claude_channel_startup("%9", "Claude Code") is True


def test_channel_startup_driver_false_on_timeout_without_marker(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *_a, **_kw: "Claude Code\n")
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.agent.AGENT_STARTUP_TIMEOUT", 0.5)
    monkeypatch.setattr("hive.agent._CHANNEL_NOTICE_GRACE", 0.01)
    monkeypatch.setattr("hive.adapters.claude_channel.is_ready", lambda _pane: False)

    assert agent_mod._drive_claude_channel_startup("%9", "Claude Code") is False


def test_spawn_claude_separates_dashed_prompt_with_double_dash(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude", skill="none", prompt="--edge prompt")

    startup_cmd = calls[0]
    # the channel flags are variadic: without `--` claude would consume the
    # positional prompt as a flag value and abort launch
    assert " -- " in startup_cmd
    assert startup_cmd.index(" -- ") < startup_cmd.index("--edge prompt")


def test_spawn_claude_resume_registers_channel(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    flagged: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_channel.prepare_pane",
        lambda cwd: flagged.append(cwd) or list(_CHANNEL_FLAGS),
    )

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude", session_id="sess-123")

    # resume/fork re-launches the claude process with the same launch flags:
    # the channel registers exactly like a fresh spawn (channel-only delivery)
    assert flagged == ["/tmp"]
    assert "--channels" in calls[0] and "plugin:hive-channel@hive" in calls[0]
    assert "sess-123" in calls[0]


def test_spawn_claude_channel_refused_fails_before_pane_creation(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    splits: list[str] = []
    monkeypatch.setattr(
        "hive.agent.tmux.split_window",
        lambda target, **_kw: splits.append(target) or target,
    )
    monkeypatch.setattr("hive.adapters.claude_channel.prepare_pane", lambda _cwd: [])

    with pytest.raises(RuntimeError, match="channel"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert splits == []  # fail-fast: no pane created for an undeliverable agent
    assert calls == []  # nothing was typed anywhere


def test_spawn_claude_startup_without_channel_notice_fails(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr(
        "hive.agent._drive_claude_channel_startup", lambda _pane, _ready: False
    )
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)
    cleared: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_channel.clear_ready", cleared.append)

    with pytest.raises(RuntimeError, match="channel"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert killed == ["%0"]  # the half-started pane is not left behind
    # cleared twice: the pre-launch stale-marker guard and the failure cleanup
    assert cleared == ["%0", "%0"]


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


def test_spawn_codex_uses_model_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="gpt-5.2", cwd="/tmp", is_first=True,
        skill="none", cli="codex",
    )

    startup_cmd = calls[0]
    assert "-m 'gpt-5.2'" in startup_cmd
    assert "codex" in startup_cmd


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
    assert "codex" in startup_cmd
    assert "fork" in startup_cmd
    assert "sess-abc" in startup_cmd
    # codex fork does not take --model; model flag should not appear
    assert "-m" not in startup_cmd


def test_spawn_codex_new_session_uses_remote_daemon(monkeypatch):
    from pathlib import Path

    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)  # daemon up + first-round runtime ready

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
    )

    startup_cmd = calls[0]
    assert "--remote" in startup_cmd
    assert "unix:///home/.codex/app-server-control/hive-pane-0.sock" in startup_cmd
    assert "--cd '/work/dir'" in startup_cmd  # codex flag is -C/--cd, not --cwd


def _mock_daemon_up(monkeypatch):
    from pathlib import Path
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.spawn_daemon", lambda *_a, **_kw: True
    )
    # Readiness polls the daemon runtime; tests answer on the first round.
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: object()
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path",
        lambda pane: Path(f"/home/.codex/app-server-control/hive-pane-{pane.replace('%', '')}.sock"),
    )


def test_spawn_codex_preconnects_2nd_client_with_workspace(monkeypatch):
    # With a workspace, spawn asks the sidecar to bring the 2nd client online
    # before codex starts, so it never has to late-join/resume.
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    connects: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda workspace, pane: connects.append((workspace, pane)) or {"ok": True},
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/work/dir", is_first=True, skill="none", cli="codex",
        workspace="/tmp/ws",
    )

    assert connects == [("/tmp/ws", "%0")]


def test_spawn_codex_skips_preconnect_without_workspace(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    connects: list = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda workspace, pane: connects.append((workspace, pane)),
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


def test_spawn_codex_resume_does_not_start_daemon(monkeypatch):
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
    assert "--remote" not in startup_cmd  # resume stays embedded
    assert started == []  # daemon not started on resume


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


def test_send_claude_accepted_passes_classification_through(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    for classification in ("mcpWriteAccepted", "legacySocketAccepted"):
        monkeypatch.setattr(
            "hive.adapters.claude_channel.send_to_pane",
            lambda pane, text, _c=classification: _c,
        )
        agent = Agent(name="w", team_name="t", pane_id="%3", cli="claude")
        assert agent.send("hi") == classification
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
    daemon_calls: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_pane",
        lambda *a: daemon_calls.append(a) or True,
    )
    channel_calls: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_channel.send_to_pane",
        lambda *a: channel_calls.append(a) or True,
    )
    submitted: list[tuple] = []
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text", lambda *a: submitted.append(a)
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert daemon_calls == []  # codex daemon path not taken for claude
    assert len(channel_calls) == 1  # claude delivers over its MCP channel
    assert submitted == []  # channel-only: no keystroke fallback


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


def test_spawn_codex_fork_contract_unchanged(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    # fork never touches the daemon; the default spawn_daemon mock returning
    # False must not matter.
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", session_id="roll-1")

    cmd = calls[0]
    assert "fork 'roll-1'" in cmd or "fork roll-1" in cmd
    assert "--remote" not in cmd
    assert "resume" not in cmd


def test_spawn_codex_resume_is_daemon_native(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    connects: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "hive.sidecar.request_connect_codex",
        lambda ws, pane: connects.append((ws, pane)),
    )

    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/repo",
                cli="codex", session_id="roll-1", session_mode="resume",
                skill="none", workspace="/ws")

    cmd = calls[0]
    assert "--remote 'unix:///home/.codex/app-server-control/hive-pane-0.sock'" in cmd
    assert "--cd '/repo'" in cmd
    assert "resume 'roll-1'" in cmd
    assert "fork" not in cmd
    assert connects == [("/ws", "%0")]


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


def test_spawn_claude_channel_readiness_skips_banner_and_settle(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    banner_waits, sleeps = _watch_banner_and_sleep(monkeypatch)

    # fresh and resume: the channel marker is the oracle, the banner (which a
    # resumed session never renders) is not consulted at all
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp", cli="claude")
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="sess-1", session_mode="resume")

    assert banner_waits == []
    assert 1 not in sleeps  # no fixed 1s settle either


def test_spawn_codex_daemon_native_waits_on_runtime_not_banner(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    banner_waits, sleeps = _watch_banner_and_sleep(monkeypatch)

    probes: list[str] = []
    runtimes = iter([None, None, object()])
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane",
        lambda pane: probes.append(pane) or next(runtimes),
    )

    Agent.spawn(name="v", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", skill="none", session_id="roll-1", session_mode="resume")

    assert banner_waits == []
    assert probes == ["%0", "%0", "%0"]  # polled until the thread appeared
    assert sleeps.count(0.5) == 2  # one interval sleep per empty round

    # fresh session, runtime present on the first round: zero sleeps
    sleeps.clear()
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: object()
    )
    Agent.spawn(name="v2", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", skill="none")
    assert banner_waits == []
    assert sleeps == []


def test_wait_codex_thread_ready_timeout_is_deterministic_and_nonfatal(monkeypatch):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: None
    )
    assert agent_mod._wait_codex_thread_ready("%9", timeout=0, interval=0) is False

    # spawn survives a readiness timeout and still completes
    _setup_tmux_mocks(monkeypatch)
    _mock_daemon_up(monkeypatch)
    monkeypatch.setattr("hive.agent._wait_codex_thread_ready", lambda _p: False)

    a = Agent.spawn(name="v", team_name="t", target_pane="%0", cwd="/tmp",
                    cli="codex", skill="hive")
    assert a.pane_id == "%0"


def test_spawn_codex_embedded_fork_keeps_banner_wait(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    banner_waits, sleeps = _watch_banner_and_sleep(monkeypatch)
    probes: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane",
        lambda pane: probes.append(pane) or object(),
    )

    Agent.spawn(name="f", team_name="t", target_pane="%0", cwd="/tmp",
                cli="codex", session_id="roll-1")  # fork mode: no daemon

    assert len(banner_waits) == 1  # legacy oracle stays for the embedded fork
    assert 1 in sleeps  # old settle preserved
    assert probes == []  # daemon runtime never consulted — flag-driven, not cli-driven


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
    assert "--remote" in startup_cmd  # daemon-native flags unchanged


def test_spawn_codex_fork_shortcut_launch_keeps_shell(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    Agent.spawn(
        name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none",
        cli="codex", session_id="sess-abc",
    )
    startup_cmd = calls[0]
    _assert_launch_keeps_shell(startup_cmd)
    assert "fork" in startup_cmd and "sess-abc" in startup_cmd
