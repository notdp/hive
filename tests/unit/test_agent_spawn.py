"""Tests for Agent.spawn model/skill/env handling."""

import json

import pytest

from hive import agent as agent_mod
from hive.adapters import claude_bg, claude_sessions
from hive.agent import (
    DeliveryError,
    Agent,
    detect_current_session_id,
)


def _fake_engine(
    pid: int = 4321,
    job_id: str = "abcd1234",
    session_id: str = "sess-registry",
) -> claude_bg.EngineSession:
    """A bg engine registry entry as engine_session_for_job would return it."""
    return claude_bg.EngineSession(
        pid=pid,
        job_id=job_id,
        session_id=session_id,
        socket_path=f"/tmp/hive-test-inbox-{pid}.sock",
        cwd="/tmp",
        status="idle",
        waiting_for="",
        status_updated_at=0.0,
    )


def _pin_job(monkeypatch, *, job_id, engine):
    """Pin the claude delivery lookup chain: pane record -> engine entry.
    Both halves read the live config tree, so every test that reaches them
    must pin them. Returns the job ids handed to engine_session_for_job."""
    seen_jobs: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _pane: job_id)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job",
        lambda jid: seen_jobs.append(jid) or engine,
    )
    return seen_jobs


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
    # The early re-tile and the engine tmux-context probe must never reach a
    # real server from a unit test.
    monkeypatch.setattr("hive.agent.tmux.get_pane_window_target", lambda _pane: "")
    monkeypatch.setattr("hive.agent.tmux.display_value", lambda _pane, _fmt: None)
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
    # Default: the claude bg spawn path succeeds without touching the real
    # claude binary or config tree. Tests observing the spawn use
    # _mock_claude_bg_up instead.
    monkeypatch.setattr("hive.adapters.claude_bg.spawn_job", lambda **_kw: "abcd1234")
    monkeypatch.setattr(
        "hive.adapters.claude_bg.wait_engine_entry",
        lambda _jid, timeout=0: _fake_engine(),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.ensure_engine",
        lambda jid, **_kw: _fake_engine(job_id=jid),
    )
    monkeypatch.setattr("hive.adapters.claude_bg.write_pane_job", lambda *_a, **_kw: None)
    monkeypatch.setattr("hive.adapters.claude_bg.stop_job", lambda *_a, **_kw: None)
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda _jid, **_kw: None)

    return calls, tags


def _mock_claude_bg_up(monkeypatch, *, job_id="abcd1234", session_id="sess-registry"):
    """Bg job path up; record spawns, wakes, pane records and stops."""
    state: dict = {"spawns": [], "wakes": [], "records": [], "stopped": []}
    engine = _fake_engine(job_id=job_id, session_id=session_id)

    def _spawn(*, cwd, name, prompt="", extra_args=None, extra_env=None, **_kw):
        state["spawns"].append({
            "cwd": cwd,
            "name": name,
            "prompt": prompt,
            "extra_args": list(extra_args or []),
            "extra_env": extra_env,
        })
        return job_id

    monkeypatch.setattr("hive.adapters.claude_bg.spawn_job", _spawn)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.wait_engine_entry", lambda _jid, timeout=0: engine
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.ensure_engine",
        lambda jid, **_kw: state["wakes"].append(jid) or engine,
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.write_pane_job",
        lambda pane, jid, sid, cwd: state["records"].append((pane, jid, sid, cwd)),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.stop_job",
        lambda jid, **_kw: state["stopped"].append(jid),
    )
    return state


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
    state = _mock_claude_bg_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="demo-review",
    )

    # The skill activation rides the bg spawn's prompt, not the pane command.
    assert state["spawns"][0]["prompt"] == "/demo-review"
    assert not any("hive teammate" in c for c in calls)


def test_spawn_skips_skill_when_none(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    assert state["spawns"][0]["prompt"] == ""
    assert not any(c.startswith("/") and not c.startswith("/tmp") for c in calls)


def test_spawn_passes_extra_env(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

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
    # The engine runs outside the pane, so the env must reach the bg spawn.
    assert state["spawns"][0]["extra_env"] == {"CR_WORKSPACE": "/tmp/cr-test"}


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
    _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.",
    )

    # Skill activation + user prompt ride the bg spawn's positional prompt.
    assert state["spawns"][0]["prompt"] == "/hive\n\nPlease check your inbox."
    assert state["spawns"][0]["name"] == "t.w1"


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


def test_spawn_claude_mints_job_records_pane_and_attaches(monkeypatch):
    # the job (and its engine entry) exist BEFORE the pane command is typed:
    # readiness is the engine registering, never screen text
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)
    captured: list = []
    monkeypatch.setattr("hive.agent.tmux.capture_pane", lambda *a, **kw: captured.append(a) or "")

    agent = Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                        is_first=True, cli="claude")

    assert agent.pane_id == "%0"
    assert captured == []  # no screen scraping anywhere in the spawn
    assert state["spawns"][0]["name"] == "t.w1"
    assert state["records"] == [("%0", "abcd1234", "sess-registry", "/tmp")]
    launch = calls[0].split(" && ")[-1].split("; hive resume-hint")[0]
    assert launch.split() == ["hive", "claude", "--resume", "'abcd1234'"]


def test_spawn_claude_mint_failure_kills_pane_and_fails(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_bg.spawn_job", lambda **_kw: None)
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)

    with pytest.raises(RuntimeError, match="job identity"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert killed == ["%0"]
    assert calls == []  # no startup command was ever sent


def test_spawn_claude_engine_never_registers_stops_job_and_fails(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.wait_engine_entry", lambda _jid, timeout=0: None
    )
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)

    with pytest.raises(RuntimeError, match="inbox-only"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude")

    assert state["stopped"] == ["abcd1234"]  # the half-born job is parked
    assert killed == ["%0"]
    assert calls == []


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


def test_spawn_claude_resume_wakes_the_job_and_rebinds_the_pane(monkeypatch):
    # resume of a claude member is just waking its durable job: nothing is
    # minted, the pane record points at the same jobId
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch, job_id="cafe0123")

    Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude", session_id="cafe0123",
                session_mode="resume")

    assert state["spawns"] == []  # nothing minted on resume
    assert state["wakes"] == ["cafe0123"]
    assert state["records"] == [("%0", "cafe0123", "sess-registry", "/tmp")]
    assert "--resume 'cafe0123'" in calls[0]


def test_spawn_claude_resume_of_a_gone_job_fails_and_gives_the_pane_back(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_bg.ensure_engine", lambda *_a, **_kw: None)
    killed: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.kill_pane", killed.append)

    with pytest.raises(RuntimeError, match="did not come back"):
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp",
                    is_first=True, cli="claude", session_id="cafe0123",
                    session_mode="resume")

    assert killed == ["%0"]
    assert calls == []


def test_spawn_tags_pane_before_waiting_for_ready(monkeypatch):
    calls, tags = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent.tmux.wait_for_texts", lambda *_args, **_kw: False)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%9",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
    )

    assert calls, "spawn should still start the CLI process"
    assert tags == [("%9", "agent", "w1", "t")]


def test_spawn_claude_pins_model_at_bg_spawn_not_pane_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="opus", cwd="/tmp", is_first=True,
        skill="none", cli="claude",
    )

    # model is a bg-spawn flag (durable in respawnFlags), not a viewer flag
    assert state["spawns"][0]["extra_args"] == ["--model", "opus"]
    assert "--model" not in calls[0]


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


def test_spawn_claude_fork_mints_a_new_job_from_the_session(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
        session_id="sess-abc",
    )

    # fork mode: a NEW bg job branches the source session server-side
    assert state["spawns"][0]["extra_args"] == ["-r", "sess-abc", "--fork-session"]
    assert "--resume 'abcd1234'" in calls[0]  # the pane attaches to the fork


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


def test_send_claude_writes_to_the_engine_inbox_as_the_member_address(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    engine = _fake_engine()
    _pin_job(monkeypatch, job_id=engine.job_id, engine=engine)
    writes: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: writes.append((sock, text, sender))
        or claude_sessions.ACCEPTED_UDS_WRITE,
    )

    accepted = Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert accepted == "udsWriteAccepted"
    assert writes == [(engine.socket_path, "hi", "t.w")]
    assert calls == []  # native transport only — the composer is never touched


def test_send_claude_resolves_the_engine_from_the_pane_job_record(monkeypatch):
    # the delivery address is derived pane -> job record -> engine entry;
    # nothing on the pane tty (the attach viewer!) is ever what gets messaged
    _pin_cli_probe(monkeypatch, "claude")
    _setup_tmux_mocks(monkeypatch)
    panes: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.job_id_for_pane",
        lambda pane: panes.append(pane) or "beef4321",
    )
    seen_jobs: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job",
        lambda jid: seen_jobs.append(jid) or _fake_engine(job_id=jid),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: claude_sessions.ACCEPTED_UDS_WRITE,
    )

    Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert panes == ["%3"]
    assert seen_jobs == ["beef4321"]  # the pane's own record keys the engine


def test_send_claude_without_job_record_raises(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    writes: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda *a, **kw: writes.append((a, kw)) or claude_sessions.ACCEPTED_UDS_WRITE,
    )

    with pytest.raises(DeliveryError, match="no bg job record"):
        Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert writes == []  # no socket to write to; nothing was attempted
    assert calls == []


def test_send_claude_asleep_engine_is_woken_then_delivered(monkeypatch):
    # a parked engine (supervisor idles jobs after ~1h) is not a dead member:
    # the ledger still lists the job, the wake revives it, delivery proceeds
    _pin_cli_probe(monkeypatch, "")  # no viewer on the pane either
    calls, _ = _setup_tmux_mocks(monkeypatch)
    engine = _fake_engine(job_id="beef4321")
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: "beef4321")
    monkeypatch.setattr("hive.adapters.claude_bg.engine_session_for_job", lambda _j: None)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.job_row", lambda _j, **_kw: {"id": "beef4321"}
    )
    wakes: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.ensure_engine",
        lambda jid, **_kw: wakes.append(jid) or engine,
    )
    writes: list[tuple] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: writes.append((sock, text, sender))
        or claude_sessions.ACCEPTED_UDS_WRITE,
    )

    accepted = Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert accepted == "udsWriteAccepted"
    assert wakes == ["beef4321"]
    assert writes == [(engine.socket_path, "hi", "t.w")]
    assert calls == []


def test_send_claude_gone_job_raises(monkeypatch):
    # the ledger no longer lists the job (removed): nothing to wake
    _pin_cli_probe(monkeypatch, "")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: "beef4321")
    monkeypatch.setattr("hive.adapters.claude_bg.engine_session_for_job", lambda _j: None)
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda _j, **_kw: None)
    wakes: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.ensure_engine",
        lambda jid, **_kw: wakes.append(jid) or None,
    )

    with pytest.raises(DeliveryError, match="gone"):
        Agent(name="w", team_name="t", pane_id="%3", cli="claude").send("hi")

    assert wakes == []  # nothing listed → no wake attempt
    assert calls == []


def test_send_claude_not_listening_raises_without_keystrokes(monkeypatch):
    _pin_cli_probe(monkeypatch, "claude")
    calls, _ = _setup_tmux_mocks(monkeypatch)
    _pin_job(monkeypatch, job_id="abcd1234", engine=_fake_engine())
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
    _pin_job(monkeypatch, job_id="abcd1234", engine=_fake_engine())
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
    _pin_job(monkeypatch, job_id="abcd1234", engine=_fake_engine())
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


def test_spawn_claude_fork_and_resume_session_semantics(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)

    # fork: a new bg job branches the source session
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="sess-1")
    assert state["spawns"][-1]["extra_args"] == ["-r", "sess-1", "--fork-session"]
    assert state["wakes"] == []

    # resume: the id is the durable jobId — wake it, mint nothing
    calls.clear()
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="cafe0123", session_mode="resume")
    assert state["wakes"] == ["cafe0123"]
    assert len(state["spawns"]) == 1  # unchanged from the fork above


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


def test_spawn_claude_engine_readiness_skips_banner_and_settle(monkeypatch):
    _setup_tmux_mocks(monkeypatch)
    banner_waits, sleeps = _watch_banner_and_sleep(monkeypatch)

    # fresh and resume: the engine's registry entry is the oracle, the banner
    # (the pane only shows an attach viewer) is not consulted at all
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp", cli="claude")
    Agent.spawn(name="w", team_name="t", target_pane="%0", cwd="/tmp",
                cli="claude", session_id="cafe0123", session_mode="resume")

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
        session_id="cafe0123", session_mode="resume",
    )
    startup_cmd = calls[0]
    _assert_launch_keeps_shell(startup_cmd)
    assert "--resume 'cafe0123'" in startup_cmd  # the pane reattaches the job


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


def test_spawn_skill_ref_is_bare_for_grok_and_qualified_for_claude(monkeypatch):
    """grok/codex register plugin skills by bare name (/hive, $hive); claude
    addresses them fully qualified (/hive:hive). /skills in grok only opens
    the picker — never format the grok launch with it."""
    calls, _ = _setup_tmux_mocks(monkeypatch)
    state = _mock_claude_bg_up(monkeypatch)
    monkeypatch.setattr("hive.adapters.grok_leader.spawn_daemon", lambda *_a, **_kw: True)
    monkeypatch.setattr("hive.agent._wait_grok_session_ready", lambda *_a, **_kw: True)

    Agent.spawn(name="g", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="grok", skill="hive:hive")
    grok_all = " ".join(calls)
    assert "/hive" in grok_all
    assert "/skills" not in grok_all and "/hive:hive" not in grok_all

    calls.clear()
    Agent.spawn(name="c", team_name="t", target_pane="%0", cwd="/tmp",
                is_first=True, cli="claude", skill="hive:hive")
    # claude's skill rides the bg spawn prompt, fully qualified
    assert state["spawns"][-1]["prompt"].startswith("/hive:hive")
