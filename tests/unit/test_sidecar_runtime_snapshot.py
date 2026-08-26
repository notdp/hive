from types import SimpleNamespace

import hive.sidecar as sidecar
from hive.runtime_snapshot import RuntimeSnapshotStore


def _aged_snapshot(store: RuntimeSnapshotStore, pane_id: str, session_id: str):
    """A snapshot written past its freshness window (the `/new` case)."""
    return store.update_session_id(
        pane_id,
        session_id,
        source="pidfile",
        observed_at=sidecar.time.monotonic() - sidecar._SESSION_SNAPSHOT_FRESHNESS_S - 1.0,
        freshness_s=sidecar._SESSION_SNAPSHOT_FRESHNESS_S,
    )


def test_runtime_snapshot_payload_reads_store_without_live_probe(monkeypatch):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-tick", source="pidfile", observed_at=10.0)
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)

    payload = sidecar._runtime_snapshot_payload("%1")

    assert payload["ok"] is True
    assert payload["pane"] == "%1"
    assert payload["snapshot"]["sessionId"] == "sid-tick"
    assert payload["snapshot"]["_sessionIdSource"] == "pidfile"


def test_runtime_snapshot_payload_reports_stale_snapshot(monkeypatch):
    store = RuntimeSnapshotStore()
    _aged_snapshot(store, "%1", "sid-old")
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)

    payload = sidecar._runtime_snapshot_payload("%1")

    assert payload["ok"] is True
    assert payload["snapshot"]["sessionId"] == "sid-old"
    assert payload["snapshot"]["_sessionIdFresh"] is False


def test_runtime_snapshot_payload_returns_none_when_snapshot_missing(monkeypatch):
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", RuntimeSnapshotStore())

    assert sidecar._runtime_snapshot_payload("%1") == {
        "ok": True,
        "pane": "%1",
        "snapshot": None,
    }


def test_resolve_transcript_path_cached_ignores_stale_snapshot_and_cached_path(
    monkeypatch,
    tmp_path,
):
    store = RuntimeSnapshotStore()
    _aged_snapshot(store, "%1", "sid-old")
    old_transcript = tmp_path / "old.jsonl"
    new_transcript = tmp_path / "new.jsonl"
    old_transcript.write_text("old")
    new_transcript.write_text("new")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%1"
            return "sid-new"

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            assert session_id == "sid-new"
            assert cwd == "/repo"
            return new_transcript

    monkeypatch.setattr(sidecar, "_TRANSCRIPT_PATH_CACHE", {
        "%1": (str(old_transcript), sidecar.time.monotonic() + 60.0, "sid-old"),
    })
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda _pane_id, _fmt: "/repo")
    monkeypatch.setattr(sidecar, "detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert sidecar._resolve_transcript_path_cached("%1") == str(new_transcript)


def test_resolve_transcript_path_cached_ignores_stale_snapshot_negative_cache(
    monkeypatch,
    tmp_path,
):
    store = RuntimeSnapshotStore()
    _aged_snapshot(store, "%1", "sid-old")
    transcript = tmp_path / "new.jsonl"
    transcript.write_text("new")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%1"
            return "sid-new"

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            assert session_id == "sid-new"
            assert cwd == "/repo"
            return transcript

    monkeypatch.setattr(sidecar, "_TRANSCRIPT_PATH_CACHE", {
        "%1": ("", sidecar.time.monotonic() + 60.0, ""),
    })
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda _pane_id, _fmt: "/repo")
    monkeypatch.setattr(sidecar, "detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert sidecar._resolve_transcript_path_cached("%1") == str(transcript)


def test_resolve_transcript_path_cached_requires_same_snapshot_session(
    monkeypatch,
    tmp_path,
):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-new", source="pidfile", observed_at=sidecar.time.monotonic())
    old_transcript = tmp_path / "old.jsonl"
    new_transcript = tmp_path / "new.jsonl"
    old_transcript.write_text("old")
    new_transcript.write_text("new")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            raise AssertionError("fresh snapshot session should be used")

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            assert session_id == "sid-new"
            assert cwd == "/repo"
            return new_transcript

    monkeypatch.setattr(sidecar, "_TRANSCRIPT_PATH_CACHE", {
        "%1": (str(old_transcript), sidecar.time.monotonic() + 60.0, "sid-old"),
    })
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda _pane_id, _fmt: "/repo")
    monkeypatch.setattr(sidecar, "detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert sidecar._resolve_transcript_path_cached("%1") == str(new_transcript)


def test_agent_runtime_payload_does_not_consume_stale_snapshot_or_pidfile(monkeypatch):
    store = RuntimeSnapshotStore()
    stale = _aged_snapshot(store, "%1", "sid-old")

    fake_profile = SimpleNamespace(name="claude")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%1"
            return None

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            raise AssertionError("stale session should not be resolved")

    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _pane_id: {"busy": False})
    monkeypatch.setattr(sidecar, "detect_cli_process_for_pane", lambda _pane_id: fake_profile)
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_args, **_kwargs: "")
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    runtime = sidecar._agent_runtime_payload("%1", runtime_snapshot=stale)

    assert runtime["sessionId"] == "unresolved"
    assert runtime["inputState"] == "unknown"
    assert runtime["inputReason"] == "no_session"


def test_agent_runtime_payload_stamps_a_freshness_window_on_a_probed_session(monkeypatch):
    # Without a window the first probed id is pinned forever: after `/new` in
    # an unmanaged pane the sidecar would keep serving the dead session.
    store = RuntimeSnapshotStore()

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            return "sid-new"

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            return None

    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda _pane_id, _fmt: "/repo")
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _pane_id: {"busy": False})
    monkeypatch.setattr(sidecar, "_claude_bg_runtime", lambda _pane_id: None)
    monkeypatch.setattr(
        sidecar, "detect_cli_process_for_pane", lambda _pane_id: SimpleNamespace(name="claude")
    )
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_args, **_kwargs: "")
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert sidecar._agent_runtime_payload("%1")["sessionId"] == "sid-new"

    field = store.get("%1").sessionId
    assert field.freshness_s == sidecar._SESSION_SNAPSHOT_FRESHNESS_S
    assert field.is_fresh(now=field.observed_at + 1.0) is True
    assert field.is_fresh(now=field.observed_at + field.freshness_s + 1.0) is False
