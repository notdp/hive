import sqlite3

from hive import bus


def test_init_workspace_creates_expected_directories(tmp_path):
    workspace = bus.init_workspace(tmp_path / "ws")

    assert workspace == tmp_path / "ws"
    for name in bus.WORKSPACE_DIRS:
        assert (workspace / name).is_dir()
    assert (workspace / bus.DB_FILENAME).is_file()


def test_init_workspace_does_not_create_cursor_table(tmp_path):
    workspace = bus.init_workspace(tmp_path / "ws")

    with sqlite3.connect(workspace / bus.DB_FILENAME) as conn:
        rows = conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'").fetchall()

    assert "cursors" not in {str(name) for (name,) in rows}


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


def test_reset_workspace_removes_sqlite_trio_before_reconnect(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    workspace.mkdir()
    db_path = workspace / bus.DB_FILENAME
    wal_path = workspace / f"{bus.DB_FILENAME}-wal"
    shm_path = workspace / f"{bus.DB_FILENAME}-shm"
    db_path.write_text("db")
    wal_path.write_text("wal")
    shm_path.write_text("shm")

    class _DummyConn:
        def close(self) -> None:
            return None

    monkeypatch.setattr("hive.bus._connect", lambda _workspace: _DummyConn())

    bus.reset_workspace(workspace)

    assert not db_path.exists()
    assert not wal_path.exists()
    assert not shm_path.exists()


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


def test_format_msg_id_is_short_and_unique_for_small_range():
    values = [bus.format_msg_id(i) for i in range(1, 2000)]

    assert all(len(value) == 4 for value in values)
    assert len(set(values)) == len(values)


def test_write_send_event_assigns_msg_id_without_followup_patch(tmp_path, monkeypatch):
    monkeypatch.setattr("hive.bus._now_iso", lambda: "2026-03-17T10:00:00Z")
    workspace = bus.init_workspace(tmp_path / "ws")

    result = bus.write_send_event(
        workspace,
        from_agent="claude",
        to_agent="orch",
        body="review complete",
        artifact="/tmp/review.md",
        reply_to="r1",
    )

    assert result.seq == 1
    assert result.msg_id == bus.format_msg_id(1)
    assert bus.read_all_events(workspace) == [{
        "msgId": result.msg_id,
        "from": "claude",
        "to": "orch",
        "intent": "send",
        "body": "review complete",
        "artifact": "/tmp/review.md",
        "inReplyTo": "r1",
        "metadata": {},
        "createdAt": "2026-03-17T10:00:00Z",
    }]


def test_init_workspace_migrates_legacy_runtime_columns(tmp_path):
    workspace = tmp_path / "ws"
    workspace.mkdir(parents=True, exist_ok=True)
    db_path = workspace / bus.DB_FILENAME
    conn = sqlite3.connect(db_path)
    conn.execute(
        """
        CREATE TABLE messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            msg_id TEXT NOT NULL DEFAULT '',
            from_agent TEXT NOT NULL,
            to_agent TEXT NOT NULL,
            intent TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            artifact TEXT NOT NULL DEFAULT '',
            in_reply_to TEXT NOT NULL DEFAULT '',
            metadata_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            inject_status TEXT NOT NULL DEFAULT '',
            turn_observed TEXT NOT NULL DEFAULT '',
            runtime_queue_state TEXT NOT NULL DEFAULT '',
            queue_source TEXT NOT NULL DEFAULT ''
        )
        """
    )
    conn.execute(
        """
        INSERT INTO messages (
            msg_id, from_agent, to_agent, intent, body, artifact,
            in_reply_to, metadata_json, created_at,
            inject_status, turn_observed, runtime_queue_state, queue_source
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            "a1b2",
            "orch",
            "claude",
            "send",
            "hello",
            "",
            "",
            "{}",
            "2026-03-17T10:00:00Z",
            "submitted",
            "pending",
            "queued",
            "capture",
        ),
    )
    conn.commit()
    conn.close()

    bus.init_workspace(workspace)

    conn = sqlite3.connect(db_path)
    rows = conn.execute("PRAGMA table_info(messages)").fetchall()
    conn.close()
    columns = {row[1] for row in rows}
    assert "inject_status" not in columns
    assert "turn_observed" not in columns
    assert "runtime_queue_state" not in columns
    assert "queue_source" not in columns
    assert bus.read_all_events(workspace) == [{
        "msgId": "a1b2",
        "from": "orch",
        "to": "claude",
        "intent": "send",
        "body": "hello",
        "metadata": {},
        "createdAt": "2026-03-17T10:00:00Z",
    }]
