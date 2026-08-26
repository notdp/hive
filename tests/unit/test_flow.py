"""Tests for hive.flow — the deterministic orchestration library.

Everything the library touches (spawn, ready gate, dispatch, bus) is
mocked at the seams flow.py actually calls; the bus reply-polling seam
uses a real workspace store where noted.
"""

from types import SimpleNamespace

import pytest

import hive.flow as flow


@pytest.fixture(autouse=True)
def _fresh_ctx(monkeypatch, tmp_path):
    """Pin a resolved context and a fast poll; reset the module singleton."""
    monkeypatch.setattr(flow, "_ctx", None)
    monkeypatch.setattr(flow, "_REPLY_POLL_SECONDS", 0.01)
    team = SimpleNamespace(name="t-x", agents={}, tmux_window="dev:0")
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team", lambda _t, required=True: ("t-x", team)
    )
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: str(tmp_path / "ws"))
    (tmp_path / "ws").mkdir(exist_ok=True)
    return team


def _wire(monkeypatch, *, ready=True, replies=None):
    """Mock the cli seams; *replies* maps dispatched msgId → reply row."""
    rec = SimpleNamespace(spawns=[], dispatches=[], msg_seq=iter(f"m{i}" for i in range(1, 99)))
    replies = replies if replies is not None else {}

    def fake_spawn(t, **kw):
        rec.spawns.append(kw)
        return SimpleNamespace(pane_id=f"%{len(rec.spawns)}")

    monkeypatch.setattr("hive.cli._spawn_team_agent", fake_spawn)
    monkeypatch.setattr("hive.cli._ensure_team_sidecar", lambda t, ws: 1)
    monkeypatch.setattr(
        "hive.cli._wait_for_peer_ready",
        lambda ws, *, team_name, agents, **kw: set() if ready else set(agents),
    )

    def fake_send(**kw):
        msg_id = next(rec.msg_seq)
        rec.dispatches.append({**kw, "msgId": msg_id})
        return {"msgId": msg_id}

    monkeypatch.setattr("hive.cli._request_send_payload", fake_send)

    def fake_find_reply(ws, *, msg_id):
        return replies.get(msg_id)

    monkeypatch.setattr("hive.bus.find_reply_to", fake_find_reply)
    return rec, replies


def test_agent_spawns_dispatches_and_returns_reply(monkeypatch):
    rec, replies = _wire(monkeypatch)
    replies["m1"] = {"body": "done, see file", "artifact": "/tmp/f.md", "msgId": "r1"}

    member = flow.agent("explore auth\nwrite findings", name="explore")

    assert member.name == "explore" and member.pane == "%1"
    assert member.summary == "done, see file"
    assert member.artifact == "/tmp/f.md"
    # spawn used the generic member bootstrap, no plugin skill
    import hive.cli as cli_mod

    spawn = rec.spawns[0]
    assert spawn["agent_name"] == "explore"
    assert spawn["prompt"] == cli_mod._member_bootstrap_prompt()
    assert spawn["skill"] == "none"
    # dispatch rode an artifact carrying the full prompt, from the flow sender
    d = rec.dispatches[0]
    assert d["sender_agent"] == "flow"
    assert d["target_agent"] == "explore"
    assert open(d["artifact"]).read() == "explore auth\nwrite findings"


def test_agent_ready_timeout_raises(monkeypatch):
    _wire(monkeypatch, ready=False)
    with pytest.raises(flow.FlowError, match="did not reach ready"):
        flow.agent("task", name="explore")


def test_agent_rejects_reserved_name(monkeypatch):
    _wire(monkeypatch)
    with pytest.raises(flow.FlowError, match="own address"):
        flow.agent("task", name="flow")


def test_ask_dispatches_followup_and_updates_member(monkeypatch):
    rec, replies = _wire(monkeypatch)
    replies["m1"] = {"body": "first", "artifact": "", "msgId": "r1"}
    replies["m2"] = {"body": "fixed", "artifact": "/tmp/v2.md", "msgId": "r2"}

    member = flow.agent("task", name="impl")
    result = member.ask("rework: handle the null case")

    assert result is member
    assert member.summary == "fixed" and member.artifact == "/tmp/v2.md"
    # short single-line follow-up rides the body, no artifact file
    d = rec.dispatches[1]
    assert d["body"] == "rework: handle the null case" and d["artifact"] == ""


def test_ask_long_prompt_rides_an_artifact(monkeypatch):
    rec, replies = _wire(monkeypatch)
    replies["m1"] = {"body": "first", "artifact": "", "msgId": "r1"}
    replies["m2"] = {"body": "ok", "artifact": "", "msgId": "r2"}

    member = flow.agent("task", name="impl")
    member.ask("line one\nline two of a long rework order")

    d = rec.dispatches[1]
    assert d["artifact"] and open(d["artifact"]).read().startswith("line one")


def test_kill_retires_pane_and_blocks_further_asks(monkeypatch, _fresh_ctx):
    rec, replies = _wire(monkeypatch)
    replies["m1"] = {"body": "done", "artifact": "", "msgId": "r1"}
    killed = []
    _fresh_ctx.agents["impl"] = SimpleNamespace(pane_id="%1", kill=lambda: killed.append("impl"))
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda w: killed.append(("layout", w)))

    member = flow.agent("task", name="impl")
    member.kill()

    assert killed == ["impl", ("layout", "dev:0")]
    assert "impl" not in _fresh_ctx.agents
    with pytest.raises(flow.FlowError, match="was killed"):
        member.ask("more")


def test_parallel_returns_in_call_order_and_serializes_spawns(monkeypatch):
    rec, replies = _wire(monkeypatch)

    def fake_find_reply(ws, *, msg_id):
        return {"body": f"done-{msg_id}", "artifact": "", "msgId": f"r-{msg_id}"}

    monkeypatch.setattr("hive.bus.find_reply_to", fake_find_reply)

    a, b = flow.parallel(
        lambda: flow.agent("task a", name="alpha"),
        lambda: flow.agent("task b", name="beta"),
    )
    assert (a.name, b.name) == ("alpha", "beta")
    assert a.summary.startswith("done-") and b.summary.startswith("done-")
    assert {s["agent_name"] for s in rec.spawns} == {"alpha", "beta"}


def test_parallel_propagates_first_error(monkeypatch):
    _wire(monkeypatch, ready=False)
    with pytest.raises(flow.FlowError):
        flow.parallel(lambda: flow.agent("t", name="x"), lambda: 42)


def test_task_artifact_never_clobbers(monkeypatch, tmp_path):
    p1 = flow._task_artifact("explore", "one")
    p2 = flow._task_artifact("explore", "two")
    assert p1 != p2
    assert open(p1).read() == "one" and open(p2).read() == "two"
