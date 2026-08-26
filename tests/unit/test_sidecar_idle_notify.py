import hive.sidecar as sidecar


WINDOW = "team-a:1"
WINDOW_B = "team-a:2"


class _BusyMonitor:
    def __init__(self, *busy_panes: str, last_output_ages: dict[str, float] | None = None):
        self.busy_panes = set(busy_panes)
        self.last_output_ages = last_output_ages or {}

    def is_busy(self, pane_id: str, *, threshold_seconds: float) -> bool:
        if pane_id in self.last_output_ages:
            return self.last_output_ages[pane_id] <= threshold_seconds
        return pane_id in self.busy_panes

    def last_output_age(self, pane_id: str) -> float | None:
        return self.last_output_ages.get(pane_id)


def _setup(
    monkeypatch,
    *,
    panes=None,
    active_window="",
    pane_windows=None,
    plugin_enabled=True,
    notify_payload=None,
    window_options=None,
    stale_cleanups=None,
):
    calls: list[tuple[str, str]] = []
    pane_window_map = pane_windows or {}
    window_option_map = window_options or {}
    cleanups_sink = stale_cleanups if stale_cleanups is not None else []
    monkeypatch.setattr(sidecar, "_idle_notify_agent_panes", lambda _team_name: list(panes or ["%1"]))
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: active_window)
    monkeypatch.setattr("hive.tmux.get_pane_window_target", lambda pane_id: pane_window_map.get(pane_id, WINDOW))
    monkeypatch.setattr(
        "hive.tmux.get_window_option",
        lambda window_target, key: window_option_map.get((window_target, key)),
    )
    monkeypatch.setattr(
        sidecar.notify_ui,
        "notify",
        lambda message, pane_id, **_kwargs: calls.append((message, pane_id)) or (notify_payload if notify_payload is not None else {}),
    )
    monkeypatch.setattr(
        sidecar.notify_ui,
        "clear_stale_notify",
        lambda window_target, agent_panes, **kwargs: cleanups_sink.append((window_target, tuple(agent_panes), kwargs)),
    )
    monkeypatch.setattr("hive.plugin_manager.is_plugin_enabled", lambda name: plugin_enabled)
    monkeypatch.setattr(sidecar, "_transcript_progressed_recently", lambda _pane_id, _threshold: None)
    return calls


def _tick(state, busy_monitor, now):
    sidecar._idle_notify_tick(
        team_name="team-a",
        session_name="dev",
        idle_notify=state,
        busy_monitor=busy_monitor,
        now=now,
    )


def test_idle_notify_first_seen_window_is_already_seen_until_new_output(monkeypatch):
    calls = _setup(monkeypatch)
    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor(), now=100.0)
    _tick(state, _BusyMonitor(), now=106.0)

    assert calls == []
    assert state == {WINDOW: {"last_busy_ts": 100.0, "notified": True, "seen_since_fire": True, "missing_ticks": 0}}


def test_idle_notify_first_seen_busy_window_can_notify_after_it_goes_idle(monkeypatch):
    calls = _setup(monkeypatch)
    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor("%1"), now=100.0)
    _tick(state, _BusyMonitor(), now=104.9)
    _tick(state, _BusyMonitor(), now=105.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True


def test_idle_notify_fires_once_after_threshold(monkeypatch):
    calls = _setup(monkeypatch)
    state: dict[str, dict[str, object]] = {WINDOW: {"last_busy_ts": 95.0, "notified": False}}

    _tick(state, _BusyMonitor(), now=100.0)
    _tick(state, _BusyMonitor(), now=101.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True


def test_idle_notify_suppressed_result_counts_as_seen(monkeypatch):
    calls = _setup(monkeypatch, notify_payload={"suppressed": True})
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 95.0, "notified": False, "seen_since_fire": True},
    }

    _tick(state, _BusyMonitor(), now=100.0)
    _tick(state, _BusyMonitor(), now=101.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW]["seen_since_fire"] is True


def test_idle_notify_busy_pane_resets_timer(monkeypatch):
    calls = _setup(monkeypatch)
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 80.0, "notified": True, "seen_since_fire": True},
    }

    _tick(state, _BusyMonitor("%1"), now=100.0)

    assert calls == []
    assert state == {
        WINDOW: {
            "last_busy_ts": 100.0,
            "notified": False,
            "seen_since_fire": True,
            "missing_ticks": 0,
            "last_busy_pane": "%1",
        }
    }


def test_idle_notify_active_window_counts_as_seen(monkeypatch):
    calls = _setup(monkeypatch, active_window=WINDOW)
    state: dict[str, dict[str, object]] = {WINDOW: {"last_busy_ts": 80.0, "notified": False}}

    _tick(state, _BusyMonitor(), now=100.0)

    assert calls == []
    assert state == {WINDOW: {"last_busy_ts": 100.0, "notified": True, "seen_since_fire": True, "missing_ticks": 0}}


def test_idle_notify_does_not_refire_until_user_sees_target(monkeypatch):
    calls = _setup(monkeypatch)
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 95.0, "notified": False, "seen_since_fire": True},
    }

    _tick(state, _BusyMonitor(), now=101.0)
    _tick(state, _BusyMonitor("%1"), now=105.0)
    _tick(state, _BusyMonitor(), now=115.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW]["seen_since_fire"] is False


def test_idle_notify_refires_after_user_sees_target_and_new_round(monkeypatch):
    calls = _setup(monkeypatch, active_window=WINDOW)
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 80.0, "notified": True, "seen_since_fire": False},
    }

    _tick(state, _BusyMonitor(), now=100.0)
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: "")
    _tick(state, _BusyMonitor("%1"), now=105.0)
    _tick(state, _BusyMonitor(), now=115.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW]["seen_since_fire"] is False


def test_idle_notify_multi_pane_window_waits_for_every_pane_idle(monkeypatch):
    calls = _setup(monkeypatch, panes=["%1", "%2"])
    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor(), now=100.0)
    _tick(state, _BusyMonitor("%1"), now=101.0)
    _tick(state, _BusyMonitor(), now=103.0)
    _tick(state, _BusyMonitor("%2"), now=104.0)
    _tick(state, _BusyMonitor(), now=108.9)
    assert calls == []
    _tick(state, _BusyMonitor(), now=109.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%2")]
    assert state[WINDOW]["notified"] is True


def test_idle_notify_tracks_windows_independently(monkeypatch):
    calls = _setup(
        monkeypatch,
        panes=["%1", "%2"],
        pane_windows={"%1": WINDOW, "%2": WINDOW_B},
    )
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 95.0, "notified": False, "seen_since_fire": True},
        WINDOW_B: {"last_busy_ts": 99.9, "notified": False, "seen_since_fire": True},
    }

    _tick(state, _BusyMonitor(), now=101.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW_B]["notified"] is False


def test_idle_notify_prunes_removed_windows_after_grace(monkeypatch):
    calls = _setup(monkeypatch, panes=["%2"], pane_windows={"%2": WINDOW_B})
    state: dict[str, dict[str, object]] = {
        WINDOW: {"last_busy_ts": 80.0, "notified": True, "seen_since_fire": True},
        WINDOW_B: {"last_busy_ts": 100.0, "notified": True, "seen_since_fire": True},
    }

    for i in range(sidecar.IDLE_NOTIFY_MISSING_PRUNE_TICKS):
        _tick(state, _BusyMonitor(), now=101.0 + i)
        if i < sidecar.IDLE_NOTIFY_MISSING_PRUNE_TICKS - 1:
            assert WINDOW in state

    assert calls == []
    assert sorted(state) == [WINDOW_B]


def test_idle_notify_transient_pane_query_failure_does_not_reset_state(monkeypatch):
    pane_available = [True]
    monkeypatch.setattr(
        sidecar,
        "_idle_notify_agent_panes",
        lambda _team_name: ["%1"] if pane_available[0] else [],
    )
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: "")
    monkeypatch.setattr("hive.tmux.get_pane_window_target", lambda pane_id: WINDOW)
    calls: list[tuple[str, str]] = []
    monkeypatch.setattr(
        sidecar.notify_ui,
        "notify",
        lambda message, pane_id, **_kwargs: calls.append((message, pane_id)) or {},
    )
    monkeypatch.setattr("hive.plugin_manager.is_plugin_enabled", lambda _n: True)
    monkeypatch.setattr(sidecar, "_transcript_progressed_recently", lambda _pane_id, _threshold: None)

    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor("%1"), now=100.0)
    _tick(state, _BusyMonitor(), now=106.0)
    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]
    assert state[WINDOW]["seen_since_fire"] is False

    pane_available[0] = False
    _tick(state, _BusyMonitor(), now=107.0)
    _tick(state, _BusyMonitor(), now=108.0)
    pane_available[0] = True

    assert state[WINDOW]["seen_since_fire"] is False
    _tick(state, _BusyMonitor(), now=120.0)
    _tick(state, _BusyMonitor(), now=130.0)

    assert calls == [(sidecar.IDLE_NOTIFY_MESSAGE, "%1")]


def test_idle_notify_existing_window_flash_keeps_rebuilt_state_locked(monkeypatch):
    calls = _setup(
        monkeypatch,
        window_options={
            (WINDOW, "hive-notify-token"): "%1:old-fire",
        },
    )
    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor("%1"), now=100.0)
    _tick(state, _BusyMonitor(), now=106.0)

    assert calls == []
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW]["seen_since_fire"] is False


def test_idle_notify_clears_notify_when_target_window_is_selected(monkeypatch):
    stale_cleanups: list[tuple[str, tuple[str, ...], dict[str, object]]] = []
    calls = _setup(
        monkeypatch,
        active_window=WINDOW,
        window_options={
            (WINDOW, "hive-notify-token"): "%1:selected-fire",
        },
        stale_cleanups=stale_cleanups,
    )
    state: dict[str, dict[str, object]] = {}

    _tick(state, _BusyMonitor(), now=100.0)

    assert calls == []
    assert stale_cleanups == [
        (
            WINDOW,
            ("%1",),
            {
                "token": "%1:selected-fire",
                "remove_attention": False,
                "source": "sidecar.active_window",
                "workspace": "",
            },
        )
    ]
    assert state[WINDOW]["notified"] is True
    assert state[WINDOW]["seen_since_fire"] is True


def test_idle_notify_reconciles_selected_notify_even_when_plugin_disabled(monkeypatch):
    stale_cleanups: list[tuple[str, tuple[str, ...], dict[str, object]]] = []
    calls = _setup(
        monkeypatch,
        active_window=WINDOW,
        plugin_enabled=False,
        window_options={(WINDOW, "hive-notify-token"): "%1:selected-fire"},
        stale_cleanups=stale_cleanups,
    )
    state: dict[str, dict[str, object]] = {WINDOW: {"last_busy_ts": 80.0, "notified": False}}

    _tick(state, _BusyMonitor(), now=100.0)

    assert calls == []
    assert stale_cleanups == [
        (
            WINDOW,
            ("%1",),
            {
                "token": "%1:selected-fire",
                "remove_attention": False,
                "source": "sidecar.active_window",
                "workspace": "",
            },
        )
    ]
    assert state == {}


def test_idle_notify_selected_cleanup_uses_real_notify_reconciler(monkeypatch):
    token = "%1:selected-fire"
    window_options = {
        (WINDOW, "hive-notify-token"): token,
        (WINDOW, "hive-notify-original-name"): "dev",
        (WINDOW, "hive-notify-hook"): "after-select-window[900001]",
        (WINDOW, "hive-notify-attention"): "/tmp/missing-attention.sh",
    }
    pane_options = {
        ("%1", "hive-notify-active"): token,
        ("%2", "hive-notify-active"): "%2:other-fire",
    }
    renamed: list[tuple[str, str]] = []

    monkeypatch.setattr(sidecar, "_idle_notify_agent_panes", lambda _team_name: ["%1", "%2"])
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: WINDOW)
    monkeypatch.setattr("hive.tmux.get_pane_window_target", lambda _pane: WINDOW)
    monkeypatch.setattr("hive.plugin_manager.is_plugin_enabled", lambda _name: False)
    monkeypatch.setattr("hive.tmux.get_window_option", lambda target, key: window_options.get((target, key)))
    monkeypatch.setattr(
        "hive.tmux.clear_window_option",
        lambda target, option: window_options.pop((target, option.lstrip("@")), None),
    )
    monkeypatch.setattr("hive.tmux.rename_window", lambda target, name: renamed.append((target, name)))
    monkeypatch.setattr("hive.tmux.get_pane_option", lambda pane, key: pane_options.get((pane, key)))
    monkeypatch.setattr(
        "hive.tmux.clear_pane_option",
        lambda pane, key: pane_options.pop((pane, key), None),
    )

    state: dict[str, dict[str, object]] = {WINDOW: {"last_busy_ts": 80.0, "notified": False}}

    _tick(state, _BusyMonitor(), now=100.0)

    assert window_options == {}
    assert pane_options == {("%2", "hive-notify-active"): "%2:other-fire"}
    assert renamed == [(WINDOW, "dev")]
    assert state == {}


def test_idle_notify_skips_and_clears_state_when_plugin_disabled(monkeypatch):
    calls = _setup(monkeypatch, plugin_enabled=False)
    state: dict[str, dict[str, object]] = {WINDOW: {"last_busy_ts": 80.0, "notified": False}}

    _tick(state, _BusyMonitor(), now=200.0)

    assert calls == []
    assert state == {}


def test_active_window_switch_does_not_rearm_for_seen_output(monkeypatch):
    """Output the user already saw on the active window must not be treated
    as fresh activity right after they switch away. Without the inactive_at
    boundary check, the inactive-window branch reads ``is_busy=True`` (the
    output is still within the 3s monitor threshold), rearms ``notified``,
    and fires a spurious idle notification ~5s later.
    """
    other_window = "team-a:99"
    pane_windows = {"%1": WINDOW}
    calls = _setup(
        monkeypatch,
        panes=["%1"],
        active_window=WINDOW,
        pane_windows=pane_windows,
    )
    state: dict[str, dict[str, object]] = {}
    debug_state: dict[str, object] = {}

    # t=100: WINDOW is active and saw real output 0.5s ago.
    monitor = _BusyMonitor(last_output_ages={"%1": 0.5})
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=100.0, debug_state=debug_state,
    )
    assert state[WINDOW]["notified"] is True

    # t=101: user switches to OTHER. Same output now 1.5s old; monitor still
    # reports busy because it's within the 3s threshold.
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: other_window)
    monitor = _BusyMonitor(last_output_ages={"%1": 1.5})
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=101.0, debug_state=debug_state,
    )
    assert state[WINDOW]["notified"] is True, "seen output must not rearm notified"

    # t=106.5: 5s past last_busy_ts and beyond the busy threshold; no fire
    # because the boundary check prevented the rearm above.
    monitor = _BusyMonitor(last_output_ages={"%1": 6.5})
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=106.5, debug_state=debug_state,
    )
    assert calls == []


def test_active_window_switch_still_rearms_for_post_switch_output(monkeypatch):
    """Dual of the regression above: real new output produced AFTER the
    user switches away must still flag busy and rearm idle notify."""
    other_window = "team-a:99"
    pane_windows = {"%1": WINDOW}
    calls = _setup(
        monkeypatch,
        panes=["%1"],
        active_window=WINDOW,
        pane_windows=pane_windows,
    )
    state: dict[str, dict[str, object]] = {}
    debug_state: dict[str, object] = {}

    # Active and quiet — set up baseline.
    monitor = _BusyMonitor(last_output_ages={"%1": 5.0})
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=100.0, debug_state=debug_state,
    )

    # User switches to OTHER at t=101. inactive_at[WINDOW] = 101.
    monkeypatch.setattr("hive.tmux.get_most_recent_client_window", lambda _session: other_window)
    monitor = _BusyMonitor(last_output_ages={"%1": 6.0})  # quiet, not busy
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=101.0, debug_state=debug_state,
    )

    # t=104: claude emits brand-new output 0.5s old. Inactive_age=3.0,
    # output_age=0.5 — fresh post-switch activity, must rearm.
    monitor = _BusyMonitor(last_output_ages={"%1": 0.5})
    sidecar._idle_notify_tick(
        team_name="team-a", session_name="dev",
        idle_notify=state, busy_monitor=monitor,
        now=104.0, debug_state=debug_state,
    )
    assert state[WINDOW]["notified"] is False, "post-switch output must rearm"
    assert state[WINDOW]["last_busy_pane"] == "%1"


def test_idle_notify_agent_panes_filters_to_live_agent_roles(monkeypatch):
    monkeypatch.setattr(
        sidecar,
        "_team_member_bindings",
        lambda _team_name: {
            "agent-a": {"role": "agent", "pane": "%1"},
            "terminal": {"role": "terminal", "pane": "%2"},
            "legacy-orch": {"role": "orchestrator", "pane": "%3"},
            "dead": {"role": "agent", "pane": "%4"},
            "dup": {"role": "agent", "pane": "%1"},
        },
    )
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: pane != "%4")
    monkeypatch.setattr(
        sidecar, "detect_cli_process_for_pane", lambda pane: object()
    )

    assert sidecar._idle_notify_agent_panes("team-a") == ["%1"]
