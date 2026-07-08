from types import SimpleNamespace

import hive.sidecar as sidecar
from hive.runtime_snapshot import RuntimeSnapshotStore


def _patch_team(monkeypatch, *, cli: str = "claude"):
    fake_team = SimpleNamespace(
        name="team-x",
        agents={
            "orch": SimpleNamespace(name="orch", pane_id="%1", cli=cli),
        },
        terminals={},
        lead_agent=lambda: None,
    )
    monkeypatch.setattr("hive.team.Team.load", lambda _team_name: fake_team)


def test_runtime_snapshot_tick_records_positive_session_hit(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda pane_id, cli_name: "sid-pid" if (pane_id, cli_name) == ("%1", "claude") else None,
    )

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    snapshot = store.get("%1")
    assert snapshot is not None
    assert snapshot.sessionId.value == "sid-pid"
    assert snapshot.sessionId.source == "pidfile"
    assert snapshot.sessionId.observed_at == 10.0


def test_runtime_snapshot_tick_keeps_previous_value_on_miss(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    previous = store.update_session_id("%1", "sid-old", source="pidfile", observed_at=9.0)
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("fresh idle snapshot should not probe")),
    )
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: False)

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    assert store.get("%1") == previous


def test_runtime_snapshot_tick_runs_low_rate_steady_probe(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    monkeypatch.setattr(sidecar, "_RUNTIME_SESSION_LAST_STEADY_PROBE_AT", {})
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: False)
    probed = []

    def _probe(pane_id, cli_name):
        probed.append((pane_id, cli_name))
        return "sid-new" if (pane_id, cli_name) == ("%1", "claude") else None

    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", _probe)

    sidecar._runtime_snapshot_tick(
        "team-x",
        store=store,
        now=10.0 + sidecar.RUNTIME_SESSION_STEADY_PROBE_SECONDS + 0.1,
    )

    snapshot = store.get("%1")
    assert snapshot is not None
    assert probed == [("%1", "claude")]
    assert snapshot.sessionId.value == "sid-new"
    assert snapshot.sessionId.source == "pidfile"


def test_runtime_snapshot_tick_throttles_steady_probe_misses(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    previous = store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    monkeypatch.setattr(sidecar, "_RUNTIME_SESSION_LAST_STEADY_PROBE_AT", {})
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: False)
    probed: list[str] = []

    def _probe(pane_id, _cli_name):
        probed.append(pane_id)
        return None

    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", _probe)

    first_due = 10.0 + sidecar.RUNTIME_SESSION_STEADY_PROBE_SECONDS + 0.1
    sidecar._runtime_snapshot_tick("team-x", store=store, now=first_due)
    sidecar._runtime_snapshot_tick("team-x", store=store, now=first_due + 1.0)

    assert probed == ["%1"]
    assert store.get("%1") == previous


def test_runtime_snapshot_tick_marks_previous_value_stale_on_recent_output_miss(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=9.0)
    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", lambda _pane_id, _cli_name: None)
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: True)

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    snapshot = store.get("%1")
    assert snapshot is not None
    assert snapshot.sessionId.value == "sid-old"
    assert snapshot.sessionId.is_fresh(now=10.0) is False


def test_runtime_snapshot_tick_refreshes_stale_snapshot_from_pidfile(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=9.0)
    store.mark_session_stale("%1", observed_at=10.0)
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda pane_id, cli_name: "sid-new" if (pane_id, cli_name) == ("%1", "claude") else None,
    )
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: False)

    sidecar._runtime_snapshot_tick("team-x", store=store, now=11.0)

    snapshot = store.get("%1")
    assert snapshot is not None
    assert snapshot.sessionId.value == "sid-new"
    assert snapshot.sessionId.is_fresh(now=11.0) is True


def test_runtime_snapshot_tick_marks_stale_then_refreshes_next_session(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=9.0)
    pidfile_results = iter([None, "sid-new"])
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda pane_id, cli_name: next(pidfile_results) if (pane_id, cli_name) == ("%1", "claude") else None,
    )
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: True)

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)
    stale = store.get("%1")
    assert stale is not None
    assert stale.sessionId.value == "sid-old"
    assert stale.sessionId.is_fresh(now=10.0) is False

    sidecar._runtime_snapshot_tick("team-x", store=store, now=11.0)
    fresh = store.get("%1")
    assert fresh is not None
    assert fresh.sessionId.value == "sid-new"
    assert fresh.sessionId.source == "pidfile"
    assert fresh.sessionId.is_fresh(now=11.0) is True


def test_runtime_snapshot_tick_seeds_empty_snapshot_with_pidfile(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", lambda *_args, **_kwargs: "sid-pidfile")

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    snapshot = store.get("%1")
    assert snapshot is not None
    assert snapshot.sessionId.value == "sid-pidfile"
    assert snapshot.sessionId.source == "pidfile"


def test_runtime_snapshot_tick_keeps_fresh_existing_snapshot_without_probe(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    previous = store.update_session_id("%1", "sid-pidfile", source="pidfile", observed_at=9.0)
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: False)
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("fresh snapshot should not probe")),
    )

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    assert store.get("%1") == previous


def test_runtime_snapshot_tick_refreshes_existing_pidfile_snapshot(monkeypatch):
    _patch_team(monkeypatch)
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-pidfile", source="pidfile", observed_at=9.0)
    monkeypatch.setattr(sidecar, "_pane_has_recent_output", lambda _pane_id: True)
    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", lambda *_args, **_kwargs: "sid-pidfile")

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    snapshot = store.get("%1")
    assert snapshot is not None
    assert snapshot.sessionId.value == "sid-pidfile"
    assert snapshot.sessionId.source == "pidfile"
    assert snapshot.sessionId.observed_at == 10.0
    assert snapshot.sessionId.is_fresh(now=10.0) is True


def test_runtime_snapshot_tick_skips_codex_capture(monkeypatch):
    _patch_team(monkeypatch, cli="codex")
    store = RuntimeSnapshotStore()
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("codex should not use pidfile")),
    )

    sidecar._runtime_snapshot_tick("team-x", store=store, now=10.0)

    assert store.get("%1") is None


def test_runtime_snapshot_payload_reads_store_without_live_probe(monkeypatch):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-tick", source="pidfile", observed_at=10.0)
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr(
        sidecar,
        "_probe_session_id_from_pidfile",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("snapshot read should not probe pidfile")),
    )

    payload = sidecar._runtime_snapshot_payload("%1")

    assert payload["ok"] is True
    assert payload["pane"] == "%1"
    assert payload["snapshot"]["sessionId"] == "sid-tick"
    assert payload["snapshot"]["_sessionIdSource"] == "pidfile"


def test_runtime_snapshot_payload_reports_stale_snapshot(monkeypatch):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    store.mark_session_stale("%1", observed_at=11.0)
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
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    store.mark_session_stale("%1", observed_at=11.0)
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
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    store.mark_session_stale("%1", observed_at=11.0)
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


def test_resolve_ack_baseline_ignores_stale_snapshot(monkeypatch, tmp_path):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    store.mark_session_stale("%1", observed_at=11.0)
    transcript = tmp_path / "new.jsonl"
    transcript.write_text("new transcript")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%1"
            return "sid-new"

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            assert session_id == "sid-new"
            assert cwd == "/repo"
            return transcript

    target = SimpleNamespace(pane_id="%1")
    monkeypatch.setattr(sidecar, "_RUNTIME_SNAPSHOTS", store)
    monkeypatch.setattr(sidecar, "detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)
    monkeypatch.setattr("hive.tmux.display_value", lambda _pane_id, _fmt: "/repo")

    path, baseline = sidecar._resolve_ack_baseline(target)

    assert path == transcript
    assert baseline == transcript.stat().st_size


def test_agent_runtime_payload_does_not_consume_stale_snapshot_or_pidfile(monkeypatch):
    store = RuntimeSnapshotStore()
    store.update_session_id("%1", "sid-old", source="pidfile", observed_at=10.0)
    stale = store.mark_session_stale("%1", observed_at=11.0)
    assert stale is not None

    fake_profile = SimpleNamespace(name="claude")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%1"
            return None

        def find_session_file(self, session_id: str, *, cwd: str | None = None):
            raise AssertionError("stale session should not be resolved")

    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _pane_id: {"busy": False})
    monkeypatch.setattr(sidecar, "detect_profile_for_pane", lambda _pane_id: fake_profile)
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_args, **_kwargs: "")
    monkeypatch.setattr("hive.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)
    monkeypatch.setattr(sidecar, "_probe_session_id_from_pidfile", lambda *_args, **_kwargs: None)

    runtime = sidecar._agent_runtime_payload("%1", runtime_snapshot=stale)

    assert runtime["sessionId"] == "unresolved"
    assert runtime["inputState"] == "unknown"
    assert runtime["inputReason"] == "no_session"
