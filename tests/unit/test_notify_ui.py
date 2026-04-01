from hive import notify_ui
import subprocess


def _mock_tmux_basics(monkeypatch):
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_window_name", lambda _pane: "dev")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_window_target", lambda _pane: "dev:1")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_option", lambda _pane, _key: "orch")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_session_name", lambda _pane: "dev")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_count", lambda _pane: 3)
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_title", lambda _pane: "[orch]")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_window", lambda _session: "dev:9")
    monkeypatch.setattr("hive.notify_ui.notify_state.record_notification", lambda *args, **kwargs: None)


def test_notify_uses_tmux_popup_for_terminal_clients(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    calls: list[tuple[str, object]] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "terminal")
    monkeypatch.setattr("hive.notify_ui.tmux.supports_popup", lambda: True)
    monkeypatch.setattr("hive.notify_ui.tmux.flash_pane_border", lambda pane, seconds=12: calls.append(("flash", pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.tmux.flash_window_status", lambda target, seconds=12: calls.append(("window", target, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_tmux_popup", lambda message, pane, seconds=12: calls.append(("popup", message, pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_overlay", lambda message, pane, seconds=12: calls.append(("overlay", message, pane, seconds)))

    payload = notify_ui.notify("回来确认", "%9")

    assert payload["clientMode"] == "terminal"
    assert payload["surface"] == "popup"
    assert ("window", "dev:1", 12) in calls
    assert ("popup", "回来确认", "%9", 12) in calls
    assert not any(call[0] == "overlay" for call in calls)


def test_notify_uses_overlay_for_control_clients(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    calls: list[tuple[str, object]] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "control")
    monkeypatch.setattr("hive.notify_ui.tmux.supports_popup", lambda: True)
    monkeypatch.setattr("hive.notify_ui.tmux.flash_pane_border", lambda pane, seconds=12: calls.append(("flash", pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.tmux.flash_window_status", lambda target, seconds=12: calls.append(("window", target, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_tmux_popup", lambda message, pane, seconds=12: calls.append(("popup", message, pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_overlay", lambda message, pane, seconds=12: calls.append(("overlay", message, pane, seconds)))

    payload = notify_ui.notify("回来确认", "%9")

    assert payload["clientMode"] == "control"
    assert payload["surface"] == "overlay"
    assert ("window", "dev:1", 12) in calls
    assert ("overlay", "回来确认", "%9", 12) in calls
    assert not any(call[0] == "popup" for call in calls)


def test_notify_is_suppressed_when_user_is_already_in_target_window(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    calls: list[tuple[str, object]] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "terminal")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_window", lambda _session: "dev:1")
    monkeypatch.setattr("hive.notify_ui.tmux.flash_pane_border", lambda pane, seconds=12: calls.append(("flash", pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.tmux.flash_window_status", lambda target, seconds=12: calls.append(("window", target, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_tmux_popup", lambda message, pane, seconds=12: calls.append(("popup", message, pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.show_overlay", lambda message, pane, seconds=12: calls.append(("overlay", message, pane, seconds)))
    monkeypatch.setattr("hive.notify_ui.notify_state.record_notification", lambda *args, **kwargs: calls.append(("record", args, kwargs)))

    payload = notify_ui.notify("回来确认", "%9")

    assert payload["surface"] == "suppressed"
    assert payload["suppressed"] is True
    assert payload["suppressionReason"] == "same_window"
    assert calls == []


def test_show_tmux_popup_uses_top_right_styled_popup(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    calls: list[list[str]] = []

    monkeypatch.setattr("hive.notify_ui._write_temp_popup_script", lambda: __import__("pathlib").Path("/tmp/hive-notify-popup.sh"))
    monkeypatch.setattr("hive.notify_ui.tmux.display_value", lambda _target, fmt: "120" if fmt == "#{window_width}" else None)
    monkeypatch.setattr(
        "hive.notify_ui.subprocess.run",
        lambda args, check=False, capture_output=True, text=True: calls.append(args) or subprocess.CompletedProcess(args, 0, "", ""),
    )

    notify_ui.show_tmux_popup("回来确认", "%9", seconds=9)

    cmd = calls[0]
    popup_width, _, _ = notify_ui._popup_geometry("回来确认", window_name="dev", agent_name="orch", pane_id="%9", seconds=9)
    assert cmd[:4] == ["tmux", "display-popup", "-t", "%9"]
    assert cmd[cmd.index("-x") + 1] == str(120 - popup_width - 1)
    assert cmd[cmd.index("-y") + 1] == "1"
    assert "-w" in cmd and cmd[cmd.index("-w") + 1] != "76%"
    assert "-h" in cmd and cmd[cmd.index("-h") + 1] != "14"
    assert "-s" in cmd and "fg=colour235,bg=colour230" in cmd
    assert "-S" in cmd and "fg=colour65,bold" in cmd
    assert " notify " in cmd


def test_popup_source_does_not_render_message_header():
    assert "Message" not in notify_ui.TMUX_POPUP_SOURCE
    assert "[Space]" in notify_ui.TMUX_POPUP_SOURCE
    assert "[Any key]" in notify_ui.TMUX_POPUP_SOURCE
    assert 'if [[ "$key" == \' \' ]]; then' in notify_ui.TMUX_POPUP_SOURCE


def test_popup_geometry_keeps_short_messages_compact():
    width, height, content_width = notify_ui._popup_geometry(
        "hello",
        window_name="hive dev",
        agent_name="notify",
        pane_id="%314",
        seconds=12,
    )

    assert 56 <= width < 72
    assert 11 <= height <= 13
    assert content_width == width - 8


def test_popup_position_falls_back_to_center_when_window_width_missing(monkeypatch):
    monkeypatch.setattr("hive.notify_ui.tmux.display_value", lambda _target, _fmt: None)

    assert notify_ui._popup_position("%9", 60) == ("C", "C")
