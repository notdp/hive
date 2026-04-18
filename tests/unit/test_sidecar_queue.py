import hive.sidecar as sidecar


def test_check_pending_keeps_followup_window_open_after_unconfirmed(monkeypatch, tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    now = 300.0
    monkeypatch.setattr(sidecar.time, "time", lambda: now)

    record = {
        "msgId": "ab12",
        "targetTranscript": str(transcript),
        "targetPane": "%1",
        "targetCli": "codex",
        "baseline": 0,
        "deadlineAt": now - 30,
        "terminalNotifiedResult": "failed",
        "terminalFollowupUntil": now + 5,
    }

    assert sidecar._check_pending(record) is None


def test_check_pending_finalizes_after_followup_window_expires(monkeypatch, tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    now = 400.0
    monkeypatch.setattr(sidecar.time, "time", lambda: now)

    record = {
        "msgId": "ab12",
        "targetTranscript": str(transcript),
        "targetPane": "%1",
        "targetCli": "codex",
        "baseline": 0,
        "deadlineAt": now - 30,
        "terminalNotifiedResult": "failed",
        "terminalFollowupUntil": now - 1,
    }

    assert sidecar._check_pending(record) == sidecar._FINALIZE_PENDING


def test_inject_exception_uses_honest_failed_wording(monkeypatch):
    sent: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        sidecar,
        "detect_profile_for_pane",
        lambda _pane_id: type("Profile", (), {"name": "codex"})(),
    )
    monkeypatch.setattr(
        "hive.agent._submit_interactive_text",
        lambda pane_id, text, cli: sent.append((pane_id, text, cli)),
    )

    sidecar._inject_exception("%1", "ab12", "orch", "failed")

    assert len(sent) == 1
    assert "failed to deliver within" in sent[0][1]
    assert "Retry only if duplicate delivery is acceptable." in sent[0][1]
    assert sent[0][2] == "codex"


def test_socket_alive_requires_matching_api_version(monkeypatch):
    monkeypatch.setattr(
        sidecar,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True},
    )
    assert sidecar._socket_alive("/tmp/ws") is False

    monkeypatch.setattr(
        sidecar,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION},
    )
    assert sidecar._socket_alive("/tmp/ws") is True


def test_sidecar_identity_requires_matching_team_and_window_id():
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION, "team": "team-b", "tmuxWindowId": "@7"},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION, "team": "team-a", "tmuxWindowId": "@9"},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "buildHash": "stale",
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "buildHash": sidecar.SIDECAR_BUILD_HASH,
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is True


def test_handle_request_ping_returns_sidecar_identity():
    response, keep_running = sidecar._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        sidecar_started_at="2026-04-17T00:00:00Z",
        pending={},
        request={"action": "ping"},
    )

    assert keep_running is True
    assert response == {
        "ok": True,
        "apiVersion": sidecar.SIDECAR_API_VERSION,
        "buildHash": sidecar.SIDECAR_BUILD_HASH,
        "team": "team-a",
        "tmuxWindow": "dev:3",
        "tmuxWindowId": "@99",
        "sidecar": {
            "pid": response["sidecar"]["pid"],
            "started_at": "2026-04-17T00:00:00Z",
            "code_hash": sidecar.SIDECAR_BUILD_HASH,
        },
    }
