import json

from hive import bus


def test_init_workspace_creates_expected_directories(tmp_path):
    workspace = bus.init_workspace(tmp_path / "ws")

    assert workspace == tmp_path / "ws"
    for name in bus.WORKSPACE_DIRS:
        assert (workspace / name).is_dir()
    assert (workspace / bus.DB_FILENAME).is_file()


def test_reset_workspace_recreates_managed_dirs_and_clears_contents(tmp_path):
    workspace = bus.init_workspace(tmp_path / "ws")
    bus.write_event(
        workspace,
        from_agent="orch",
        to_agent="claude",
        intent="send",
        message_id="old1",
        body="old",
    )
    (workspace / "artifacts" / "note.txt").write_text("artifact")
    (workspace / "state" / "mode").write_text("busy")
    # Legacy dirs are cleaned up on reset.
    (workspace / "status" / "legacy.json").parent.mkdir(parents=True, exist_ok=True)
    (workspace / "status" / "legacy.json").write_text('{"state":"done"}')
    (workspace / "presence" / "team.json").parent.mkdir(parents=True, exist_ok=True)
    (workspace / "presence" / "team.json").write_text('{"team":"dev"}')
    (workspace / "keep.txt").write_text("keep")

    bus.reset_workspace(workspace)

    for name in bus.WORKSPACE_DIRS:
        root = workspace / name
        assert root.is_dir()
        assert list(root.iterdir()) == []
    assert (workspace / bus.DB_FILENAME).is_file()
    assert bus.read_all_events(workspace) == []
    assert not (workspace / "status").exists()
    assert not (workspace / "presence").exists()
    assert not (workspace / "events").exists()
    assert not (workspace / "cursors").exists()
    assert (workspace / "keep.txt").read_text() == "keep"


def test_parse_key_value_parses_and_overwrites_later_values():
    payload = bus.parse_key_value(["repo=owner/repo", "stage=1", "stage=2"])

    assert payload == {"repo": "owner/repo", "stage": "2"}


def test_parse_key_value_rejects_invalid_entries():
    try:
        bus.parse_key_value(["missing-separator"])
    except ValueError as exc:
        assert "invalid KEY=VALUE entry" in str(exc)
    else:
        raise AssertionError("expected ValueError")

    try:
        bus.parse_key_value([" =value"])
    except ValueError as exc:
        assert "empty key" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_write_event_round_trip(tmp_path, monkeypatch):
    monkeypatch.setattr("hive.bus._now_iso", lambda: "2026-03-17T10:00:00Z")
    workspace = bus.init_workspace(tmp_path / "ws")

    seq = bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="orch",
        intent="send",
        message_id="ab12",
        body="review complete",
        artifact="/tmp/review.md",
        metadata={"verdict": "issues"},
    )

    assert seq == 1
    assert bus.read_all_events(workspace) == [{
        "msgId": "ab12",
        "from": "claude",
        "to": "orch",
        "intent": "send",
        "body": "review complete",
        "artifact": "/tmp/review.md",
        "metadata": {"verdict": "issues"},
        "createdAt": "2026-03-17T10:00:00Z",
    }]


def test_write_event_multiple_events(tmp_path, monkeypatch):
    times = iter(["2026-03-17T10:00:00Z", "2026-03-17T10:00:01Z"])
    monkeypatch.setattr("hive.bus._now_iso", lambda: next(times))
    workspace = bus.init_workspace(tmp_path / "ws")

    bus.write_event(
        workspace,
        from_agent="orch",
        to_agent="claude",
        intent="send",
        message_id="aa01",
        body="review this diff",
    )
    bus.write_event(
        workspace,
        from_agent="orch",
        to_agent="gpt",
        intent="send",
        message_id="bb02",
        body="pick a strategy",
    )

    events = bus.read_all_events(workspace)
    assert [event["msgId"] for event in events] == ["aa01", "bb02"]
    assert events[0]["body"] == "review this diff"
    assert events[1]["body"] == "pick a strategy"
