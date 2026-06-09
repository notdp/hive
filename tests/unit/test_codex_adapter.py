from __future__ import annotations

import json
from pathlib import Path

from hive import adapters


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")


def test_extract_context_snapshot_reads_latest_token_count(tmp_path):
    path = tmp_path / "codex.jsonl"
    _write_jsonl(
        path,
        [
            {
                "timestamp": "2026-05-09T08:15:09.429Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 225016,
                            "cached_input_tokens": 222080,
                            "output_tokens": 914,
                            "reasoning_output_tokens": 376,
                            "total_tokens": 225930,
                        },
                        "model_context_window": 258400,
                    },
                },
            },
            {
                "timestamp": "2026-05-09T08:16:36.978Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 0,
                            "cached_input_tokens": 0,
                            "output_tokens": 0,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 24028,
                        },
                        "model_context_window": 258400,
                    },
                },
            },
            {
                "timestamp": "2026-05-09T08:16:37.000Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": []},
            },
        ],
    )

    snapshot = adapters.get("codex").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.tokens == 24028
    assert snapshot.window == 258400
    assert snapshot.observed_at is not None
    assert snapshot.observed_at.isoformat() == "2026-05-09T08:16:36.978000+00:00"
    assert snapshot.source == "codex_token_count_event"


def test_resolve_current_session_id_prefers_daemon(monkeypatch):
    """Daemon-backed codex: the session id comes from the app-server daemon, and
    the tty lsof path is never touched (it can't see the daemon-held jsonl)."""
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda pane: "sess-daemon" if pane == "%5" else None,
    )

    def _must_not_lsof(*_a, **_k):
        raise AssertionError("daemon answered — must not lsof the tty processes")

    monkeypatch.setattr("hive.adapters.codex.tmux.get_pane_tty", _must_not_lsof)

    assert adapters.get("codex").resolve_current_session_id("%5") == "sess-daemon"


def test_resolve_current_session_id_embedded_falls_back_to_tty_lsof(monkeypatch, tmp_path):
    """Embedded codex (no daemon socket): the daemon lookup yields nothing, so
    the resolver lsofs the pane's own codex process, which holds the rollout
    jsonl directly."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda _pane: None,  # no daemon to ask
    )
    monkeypatch.setattr("hive.adapters.codex.tmux.get_pane_tty", lambda _pane: "/dev/ttys003")

    from hive import tmux as _tmux

    monkeypatch.setattr(
        "hive.adapters.codex.tmux.list_tty_processes",
        lambda _tty: [_tmux.TTYProcessInfo(pid="321", command="codex", argv="codex")],
    )
    uuid = "abcdef01-2345-6789-abcd-ef0123456789"
    fpath = str(tmp_path / "sessions" / "2026" / "06" / "09" / f"rollout-2026-06-09T10-00-00-{uuid}.jsonl")
    monkeypatch.setattr("hive.adapters.codex.tmux.list_open_files", lambda _pid: [fpath])

    assert adapters.get("codex").resolve_current_session_id("%9") == uuid
