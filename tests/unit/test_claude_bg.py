"""Unit tests for the claude bg-job adapter (records, registry, ledger)."""
import json
import os

import pytest

from hive.adapters import claude_bg as m

pytestmark = pytest.mark.unit


def _claude_home(monkeypatch, tmp_path):
    home = tmp_path / "claude-home"
    monkeypatch.setenv("CLAUDE_HOME", str(home))
    return home


def _write_registry_entry(home, file_pid, **fields):
    (home / "sessions").mkdir(parents=True, exist_ok=True)
    (home / "sessions" / f"{file_pid}.json").write_text(json.dumps(fields))


def _bg_entry(pid, job_id, *, sock, status="idle", **extra):
    return dict(
        pid=pid,
        kind="bg",
        jobId=job_id,
        sessionId=f"{job_id}-ffff-4aaa-8bbb-000000000000",
        messagingSocketPath=sock,
        status=status,
        statusUpdatedAt=1_700_000_000_000,
        **extra,
    )


# --- pane <-> job records -----------------------------------------------------


def test_pane_job_record_roundtrip_and_reverse_lookup(monkeypatch, tmp_path):
    _claude_home(monkeypatch, tmp_path)

    m.write_pane_job("%19", "cafe1234", "sess-19", "/w/a")
    m.write_pane_job("%7", "beef5678", "sess-7", "/w/b")

    assert m.read_pane_job("%19") == ("cafe1234", "sess-19", "/w/a")
    assert m.job_id_for_pane("%7") == "beef5678"
    assert sorted(m.list_recorded_panes()) == ["%19", "%7"]
    assert m.pane_for_job("cafe1234") == "%19"
    assert m.pane_for_job("missing") is None
    assert m.pane_for_job("") is None

    m.clear_pane_job("%19")
    assert m.read_pane_job("%19") is None
    assert m.pane_for_job("cafe1234") is None


def test_read_pane_job_rejects_garbage(monkeypatch, tmp_path):
    home = _claude_home(monkeypatch, tmp_path)
    path = m.pane_job_path("%3")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json")
    assert m.read_pane_job("%3") is None
    path.write_text(json.dumps({"cwd": "/w"}))  # no jobId
    assert m.read_pane_job("%3") is None
    assert home  # keep the fixture explicit


def test_looks_like_job_id():
    assert m.looks_like_job_id("7fcc705f")
    assert not m.looks_like_job_id("74e0fe8d-3278-436a-98f1-7dd32c817571")
    assert not m.looks_like_job_id("worker")
    assert not m.looks_like_job_id("")


# --- engine registry entries --------------------------------------------------


def test_engine_session_for_job_finds_live_bg_entry(monkeypatch, tmp_path):
    home = _claude_home(monkeypatch, tmp_path)
    sock = tmp_path / "engine.sock"
    sock.write_text("")
    me = os.getpid()
    _write_registry_entry(home, me, **_bg_entry(me, "cafe1234", sock=str(sock), status="busy"))
    # an interactive entry never answers a job lookup
    _write_registry_entry(
        home, 424242,
        pid=me, kind="interactive", name="x", messagingSocketPath=str(sock),
    )

    engine = m.engine_session_for_job("cafe1234")
    assert engine is not None
    assert engine.pid == me
    assert engine.status == "busy"
    assert engine.socket_path == str(sock)
    assert engine.session_id.startswith("cafe1234")
    assert m.engine_session_for_job("other000") is None


def test_engine_entry_requires_live_pid_and_socket(monkeypatch, tmp_path):
    home = _claude_home(monkeypatch, tmp_path)
    sock = tmp_path / "engine.sock"
    sock.write_text("")
    dead = 4_000_000
    _write_registry_entry(home, dead, **_bg_entry(dead, "dead0001", sock=str(sock)))
    me = os.getpid()
    _write_registry_entry(
        home, me, **_bg_entry(me, "nosock01", sock=str(tmp_path / "gone.sock"))
    )

    assert m.engine_session_for_job("dead0001") is None
    assert m.engine_session_for_job("nosock01") is None
    assert m.pane_engine_alive("%1") is False


def test_session_id_for_pane_prefers_live_engine_over_record(monkeypatch, tmp_path):
    home = _claude_home(monkeypatch, tmp_path)
    sock = tmp_path / "engine.sock"
    sock.write_text("")
    me = os.getpid()
    m.write_pane_job("%5", "cafe1234", "sess-old", "/w")
    _write_registry_entry(home, me, **_bg_entry(me, "cafe1234", sock=str(sock)))

    # live engine's sessionId (follows /clear) wins over the record snapshot
    assert m.session_id_for_pane("%5").startswith("cafe1234")

    (home / "sessions" / f"{me}.json").unlink()
    # parked engine: fall back to the record's spawn-time snapshot
    assert m.session_id_for_pane("%5") == "sess-old"


# --- ledger / lifecycle -------------------------------------------------------


def test_job_row_separates_asleep_from_gone(monkeypatch):
    rows = [
        {"id": "cafe1234", "kind": "background", "state": "stopped", "sessionId": "s-1"},
        {"pid": 1, "kind": "interactive", "name": "x"},
    ]
    monkeypatch.setattr(m, "list_jobs", lambda **_kw: rows)
    assert m.job_row("cafe1234")["state"] == "stopped"  # asleep, not dead
    assert m.job_row("gone0001") is None
    assert m.job_exists("cafe1234") is True

    monkeypatch.setattr(m, "list_jobs", lambda **_kw: None)  # CLI failure
    assert m.job_row("cafe1234") is None


def test_spawn_job_parses_the_backgrounded_announcement(monkeypatch, tmp_path):
    _claude_home(monkeypatch, tmp_path)
    seen: dict = {}

    class _Result:
        returncode = 0
        stdout = "backgrounded · 7fcc705f · probe-mouse\n  claude agents  list sessions\n"
        stderr = "Starting background service…\n"

    def _run(argv, **kw):
        seen["argv"] = argv
        seen["cwd"] = kw.get("cwd")
        seen["env"] = kw.get("env")
        return _Result()

    monkeypatch.setattr(m.subprocess, "run", _run)
    monkeypatch.setenv("CLAUDE_CODE_CHILD_SESSION", "1")
    monkeypatch.setenv("ANTHROPIC_MODEL", "x")

    job_id = m.spawn_job(
        cwd="/w", name="t.w1", prompt="/hive", extra_args=["--model", "opus"],
        extra_env={"K": "V"},
    )

    assert job_id == "7fcc705f"
    assert seen["argv"] == ["claude", "--bg", "--name", "t.w1", "--model", "opus", "/hive"]
    assert seen["cwd"] == "/w"
    # env washed: an inherited child-session marker would make the engine
    # skip registration entirely; the config-tree override survives
    assert "CLAUDE_CODE_CHILD_SESSION" not in seen["env"]
    assert "ANTHROPIC_MODEL" not in seen["env"]
    assert seen["env"]["CLAUDE_CONFIG_DIR"] == str(tmp_path / "claude-home")
    assert seen["env"]["K"] == "V"


def test_spawn_job_returns_none_on_failure(monkeypatch, tmp_path):
    _claude_home(monkeypatch, tmp_path)

    class _Bad:
        returncode = 1
        stdout = ""
        stderr = "boom"

    monkeypatch.setattr(m.subprocess, "run", lambda *_a, **_kw: _Bad())
    assert m.spawn_job(cwd="/w", name="t.w1") is None


def test_ensure_engine_wakes_a_parked_job_once(monkeypatch, tmp_path):
    _claude_home(monkeypatch, tmp_path)
    engines = iter([None, "ENGINE"])
    monkeypatch.setattr(m, "engine_session_for_job", lambda _j: next(engines))
    wakes: list[str] = []
    monkeypatch.setattr(m, "wake_job", lambda jid, **_kw: wakes.append(jid) or True)

    assert m.ensure_engine("cafe1234", timeout=0) == "ENGINE"
    assert wakes == ["cafe1234"]


def test_ensure_engine_gives_up_when_wake_fails(monkeypatch, tmp_path):
    _claude_home(monkeypatch, tmp_path)
    monkeypatch.setattr(m, "engine_session_for_job", lambda _j: None)
    monkeypatch.setattr(m, "wake_job", lambda _jid, **_kw: False)
    assert m.ensure_engine("cafe1234", timeout=0) is None


# --- runtime mapping ----------------------------------------------------------


def _engine(status, *, waiting_for="", updated_at=None):
    import time as _time

    return m.EngineSession(
        pid=1, job_id="cafe1234", session_id="s", socket_path="/s", cwd="",
        status=status, waiting_for=waiting_for,
        status_updated_at=(_time.time() if updated_at is None else updated_at),
    )


def test_runtime_from_engine_maps_status_vocabulary():
    busy = m.runtime_from_engine(_engine("busy"))
    assert (busy["busy"], busy["inputState"]) == (True, "ready")

    idle = m.runtime_from_engine(_engine("idle"))
    assert (idle["busy"], idle["inputState"]) == (False, "ready")

    waiting = m.runtime_from_engine(_engine("waiting", waiting_for="input needed"))
    assert waiting["busy"] is False
    assert waiting["inputState"] == "waiting_user"
    assert waiting["inputReason"] == "registry:input needed"

    unknown = m.runtime_from_engine(_engine(""))
    assert unknown["inputState"] == "unknown"
    assert unknown["inputReason"] == "no_registry_status"


def test_runtime_from_engine_demotes_stale_status():
    stale = m.runtime_from_engine(
        _engine("busy", updated_at=1.0), now=m.STATUS_STALE_AFTER_SECONDS + 100.0
    )
    assert stale["busy"] is False
    assert stale["inputState"] == "unknown"
    assert stale["inputReason"] == "stale_status"


# --- tool-side identity (engine env -> pane) ----------------------------------


def test_member_env_pane_resolves_engine_socket_to_recorded_pane(monkeypatch, tmp_path):
    from hive import tmux

    home = _claude_home(monkeypatch, tmp_path)
    sock = tmp_path / "cc.sock"
    sock.write_text("")
    me = os.getpid()
    _write_registry_entry(home, me, **_bg_entry(me, "cafe1234", sock=str(sock)))
    m.write_pane_job("%23", "cafe1234", "s", "/w")
    monkeypatch.delenv("TMUX", raising=False)
    monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setenv("CLAUDE_CODE_MESSAGING_SOCKET", f"/tmp/cc-socks/{me}.sock")

    assert tmux.get_current_pane_id() == "%23"
    # the identity also satisfies the tmux gate for the engine's tools
    assert tmux.is_inside_tmux() is True


def test_member_env_pane_ignores_interactive_sessions(monkeypatch, tmp_path):
    from hive import tmux

    home = _claude_home(monkeypatch, tmp_path)
    me = os.getpid()
    _write_registry_entry(
        home, me, pid=me, kind="interactive", name="x",
        sessionId="s", messagingSocketPath="/tmp/x.sock",
    )
    monkeypatch.delenv("TMUX", raising=False)
    monkeypatch.setenv("CLAUDE_CODE_MESSAGING_SOCKET", f"/tmp/cc-socks/{me}.sock")
    monkeypatch.setenv("TMUX_PANE", "%88")

    # an interactive claude's tool falls through to its own TMUX_PANE
    assert tmux.get_current_pane_id() == "%88"


def test_member_env_pane_trusts_pinned_tmux_pane_without_tmux(monkeypatch):
    """Regression: a grok leader pins TMUX_PANE (no $TMUX, no per-CLI marker);
    post the env wash its tools failed the root gate and members hand-forged
    their own $TMUX to crawl past it."""
    from types import SimpleNamespace

    from hive import tmux as tmux_mod

    monkeypatch.delenv("TMUX", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_MESSAGING_SOCKET", raising=False)
    monkeypatch.setenv("TMUX_PANE", "%42")
    monkeypatch.setattr(
        tmux_mod, "_run",
        lambda args, check=False, **kw: SimpleNamespace(stdout="%42\n", returncode=0),
    )
    assert tmux_mod._member_env_pane() == "%42"
    assert tmux_mod.is_inside_tmux() is True
    assert tmux_mod.get_current_pane_id() == "%42"


def test_member_env_pane_rejects_dead_pinned_pane(monkeypatch):
    from types import SimpleNamespace

    from hive import tmux as tmux_mod

    monkeypatch.delenv("TMUX", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_MESSAGING_SOCKET", raising=False)
    monkeypatch.setenv("TMUX_PANE", "%dead")
    monkeypatch.setattr(
        tmux_mod, "_run",
        lambda args, check=False, **kw: SimpleNamespace(stdout="", returncode=1),
    )
    assert tmux_mod._member_env_pane() is None
    assert tmux_mod.is_inside_tmux() is False
