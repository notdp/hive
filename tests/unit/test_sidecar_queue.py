import json

import hive.sidecar as sidecar


def test_detect_runtime_queue_state_reads_claude_queue_event(tmp_path):
    transcript = tmp_path / "claude.jsonl"
    transcript.write_text(
        json.dumps(
            {
                "type": "queue-operation",
                "operation": "enqueue",
                "content": "<HIVE from=momo to=orch msgId=ab12>hello</HIVE>",
            }
        )
        + "\n"
    )

    result = sidecar.detect_runtime_queue_state(
        pane_id="",
        message_id="ab12",
        queue_probe_text="hello",
        transcript_path=str(transcript),
        baseline=0,
        cli_name="claude",
    )

    assert result["state"] == "queued"
    assert result["source"] == "transcript"


def test_detect_runtime_queue_state_reads_capture_phrase_for_codex_and_droid(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux.capture_pane",
        lambda _pane, lines=200: (
            "Messages to be submitted after next tool call\n↳ hello from queue preview\n"
        ),
    )

    codex = sidecar.detect_runtime_queue_state(
        pane_id="%1",
        message_id="ab12",
        queue_probe_text="hello from queue preview",
        transcript_path="",
        baseline=0,
        cli_name="codex",
    )
    assert codex["state"] == "queued"
    assert codex["source"] == "capture"

    monkeypatch.setattr(
        "hive.tmux.capture_pane",
        lambda _pane, lines=200: "Queued messages:\nhello from droid queue\n",
    )
    droid = sidecar.detect_runtime_queue_state(
        pane_id="%2",
        message_id="ab12",
        queue_probe_text="hello from droid queue",
        transcript_path="",
        baseline=0,
        cli_name="droid",
    )
    assert droid["state"] == "queued"
    assert droid["source"] == "capture"


def test_check_pending_stays_pending_while_queue_visible(monkeypatch, tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    now = 100.0
    monkeypatch.setattr(sidecar.time, "time", lambda: now)
    monkeypatch.setattr(
        sidecar,
        "detect_runtime_queue_state",
        lambda **_kw: {"state": "queued", "source": "capture", "observedAt": "2026-04-14T00:00:00Z"},
    )

    record = {
        "msgId": "ab12",
        "targetTranscript": str(transcript),
        "targetPane": "%1",
        "targetCli": "codex",
        "baseline": 0,
        "deadlineAt": now - 1,
        "queueProbeText": "hello from queue preview",
    }

    assert sidecar._check_pending(record) is None
    assert record["runtimeQueueState"] == "queued"
    assert record["lastQueuedAt"] == now


def test_check_pending_uses_post_queue_timeout_after_queue_disappears(monkeypatch, tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    now = 200.0
    monkeypatch.setattr(sidecar.time, "time", lambda: now)
    monkeypatch.setattr(
        sidecar,
        "detect_runtime_queue_state",
        lambda **_kw: {"state": "not_queued", "source": "capture", "observedAt": "2026-04-14T00:00:00Z"},
    )

    record = {
        "msgId": "ab12",
        "targetTranscript": str(transcript),
        "targetPane": "%1",
        "targetCli": "codex",
        "baseline": 0,
        "deadlineAt": now + 999,
        "runtimeQueueState": "queued",
        "lastQueuedAt": now - sidecar.POST_QUEUE_TIMEOUT - 1,
        "queueProbeText": "hello from queue preview",
    }

    assert sidecar._check_pending(record) == "unconfirmed"
