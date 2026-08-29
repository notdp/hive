"""Headless spawn: hive spawn -t from outside tmux — engine first, no pane."""
import json

from hive.cli import cli


def _headless_team(monkeypatch):
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="", created_at="1.0",
    ) == "written"
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: False)


def test_headless_spawn_codex_thread_and_bootstrap(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _headless_team(monkeypatch)
    calls = []
    cas = "hive.adapters.codex_app_server"
    monkeypatch.setattr(f"{cas}.spawn_daemon", lambda **_kw: True)
    monkeypatch.setattr(f"{cas}.ensure_dir_trusted", lambda cwd: calls.append(("trust", cwd)))
    monkeypatch.setattr(
        f"{cas}.start_member_thread",
        lambda cwd, name, model="": calls.append(("mint", name, model)) or "tid-9",
    )
    monkeypatch.setattr(
        f"{cas}.send_to_thread",
        lambda tid, text: calls.append(("boot", tid, text)) or "turnStartAccepted",
    )

    result = runner.invoke(cli, ["spawn", "rex", "-t", "honey", "--cli", "codex"])

    assert result.exit_code == 0, result.output
    assert "headless" in result.output
    assert ("mint", "honey.rex", "") in calls
    boot = next(c for c in calls if c[0] == "boot")
    assert boot[1] == "tid-9"
    assert boot[2].startswith("$hive")
    from hive import registry

    rows = {m["name"]: m for m in registry.load("honey")["members"]}
    assert rows["rex"]["cli"] == "codex"
    assert rows["rex"]["sessionId"] == "tid-9"


def test_headless_spawn_grok_session_and_bootstrap(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _headless_team(monkeypatch)
    calls = []
    gl = "hive.adapters.grok_leader"
    minted = {}

    def _create(team, member, session_id, cwd):
        minted["sid"] = session_id
        calls.append(("create", team, member, cwd))
        return True

    monkeypatch.setattr(f"{gl}.create_member_session", _create)
    monkeypatch.setattr(
        f"{gl}.send_to_key",
        lambda key, text: calls.append(("boot", key, text)) or "sessionPromptQueued",
    )

    result = runner.invoke(cli, ["spawn", "lulu", "-t", "honey", "--cli", "grok"])

    assert result.exit_code == 0, result.output
    create = next(c for c in calls if c[0] == "create")
    assert create[1:3] == ("honey", "lulu")
    boot = next(c for c in calls if c[0] == "boot")
    assert boot[1] == "m-honey.lulu"
    assert boot[2].startswith("/hive")
    from hive import registry

    rows = {m["name"]: m for m in registry.load("honey")["members"]}
    assert rows["lulu"]["sessionId"] == minted["sid"]


def test_headless_spawn_claude_bg_job(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _headless_team(monkeypatch)
    from types import SimpleNamespace

    spawns = []
    cb = "hive.adapters.claude_bg"
    monkeypatch.setattr(
        f"{cb}.spawn_job",
        lambda **kw: spawns.append(kw) or "job-7",
    )
    monkeypatch.setattr(
        f"{cb}.wait_engine_entry",
        lambda job, timeout=0: SimpleNamespace(session_id="sess-7", socket_path="/tmp/s"),
    )

    result = runner.invoke(cli, ["spawn", "opus", "-t", "honey"])

    assert result.exit_code == 0, result.output
    assert spawns[0]["name"] == "honey.opus"
    assert spawns[0]["prompt"].startswith("/hive")
    assert spawns[0]["extra_env"]["HIVE_TEAM"] == "honey"
    assert spawns[0]["extra_env"]["HIVE_MEMBER"] == "opus"
    from hive import registry

    rows = {m["name"]: m for m in registry.load("honey")["members"]}
    assert rows["opus"]["sessionId"] == "job-7"


def test_headless_spawn_grok_refuses_model(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _headless_team(monkeypatch)
    result = runner.invoke(cli, ["spawn", "g", "-t", "honey", "--cli", "grok", "-m", "grok-4.6"])
    assert result.exit_code != 0
    assert "--model" in result.output or "model" in result.output


def test_headless_spawn_duplicate_name_refused(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="", created_at="1.0",
        members=[{"name": "rex", "cli": "codex", "sessionId": "tid-1"}],
    ) == "written"
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["spawn", "rex", "-t", "honey", "--cli", "codex"])
    assert result.exit_code != 0
    assert "already exists" in result.output


def test_kill_headless_member_by_qualified_address(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="", created_at="1.0",
        members=[{"name": "rex", "cli": "grok", "sessionId": "sid-g", "cwd": "/repo"}],
    ) == "written"
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: False)
    killed = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.kill_daemon_key", lambda key: killed.append(key)
    )

    result = runner.invoke(cli, ["kill", "honey.rex"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["member"] == "rex"
    assert killed == ["m-honey.rex"]
    assert registry.load("honey")["members"] == []  # roster row removed
