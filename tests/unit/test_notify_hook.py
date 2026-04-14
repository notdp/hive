from hive import notify_hook


def test_notification_hook_uses_payload_message():
    assert notify_hook.classify_hook_payload({
        "hook_event_name": "Notification",
        "message": "Droid is waiting for your input",
    }) == ("waiting_input", "Droid is waiting for your input")


def test_stop_hook_uses_generic_completion_message():
    assert notify_hook.classify_hook_payload({"hook_event_name": "Stop"}) == (
        "completed",
        "Droid finished responding. Return to the pane to review the result.",
    )


def test_stop_hook_skips_when_stop_hook_is_active():
    assert notify_hook.classify_hook_payload({
        "hook_event_name": "Stop",
        "stop_hook_active": True,
    }) is None


def test_handle_hook_payload_suppresses_recent_agent_notification(monkeypatch):
    monkeypatch.setattr("hive.notify_hook.resolve_target_pane", lambda: "%9")
    monkeypatch.setattr("hive.notify_hook.notify_state.should_suppress_hook_notification", lambda *args, **kwargs: True)
    calls: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.notify_hook.notify_ui.notify",
        lambda message, pane_id, **kwargs: calls.append((message, pane_id, kwargs.get("source", ""))),
    )

    result = notify_hook.handle_hook_payload({"hook_event_name": "Notification", "message": "wait"})

    assert result == 0
    assert calls == []


def test_handle_hook_payload_emits_hook_notification(monkeypatch):
    monkeypatch.setattr("hive.notify_hook.resolve_target_pane", lambda: "%9")
    monkeypatch.setattr("hive.notify_hook.notify_state.should_suppress_hook_notification", lambda *args, **kwargs: False)
    calls: list[tuple[str, str, str, str]] = []
    monkeypatch.setattr(
        "hive.notify_hook.notify_ui.notify",
        lambda message, pane_id, **kwargs: calls.append((message, pane_id, kwargs.get("source", ""), kwargs.get("kind", ""))),
    )

    result = notify_hook.handle_hook_payload({"hook_event_name": "Stop"})

    assert result == 0
    assert calls == [(
        "Droid finished responding. Return to the pane to review the result.",
        "%9",
        "hook",
        "completed",
    )]


def test_resolve_target_pane_returns_empty_without_tmux_pane(monkeypatch):
    monkeypatch.delenv("TMUX_PANE", raising=False)

    assert notify_hook.resolve_target_pane() == ""


def test_resolve_target_pane_returns_tmux_pane(monkeypatch):
    monkeypatch.setenv("TMUX_PANE", "%11")

    assert notify_hook.resolve_target_pane() == "%11"
