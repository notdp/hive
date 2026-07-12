import json
import os
from pathlib import Path

import pytest

from hive import notify_debug, notify_ui


@pytest.fixture
def isolated_global_log(tmp_path, monkeypatch):
    log = tmp_path / "global-notify.jsonl"
    monkeypatch.setattr(notify_debug, "_GLOBAL_LOG", log)
    return log


def test_emit_falls_back_to_global_log_when_no_workspace(isolated_global_log):
    notify_debug.emit("", "global.event", payload="x")
    record = json.loads(isolated_global_log.read_text().splitlines()[0])
    assert record["event"] == "global.event"
    assert record["component"] == "notify"
    assert record["workspace"] == "<global>"
    assert record["payload"] == "x"
    assert record["pid"] == os.getpid()


def test_emit_filters_heartbeat_events_in_normal_mode(isolated_global_log, monkeypatch):
    monkeypatch.setenv("HIVE_LOG_VERBOSITY", "normal")

    notify_debug.emit("", "tick.summary", team="team-a")

    assert not isolated_global_log.exists()


def test_emit_writes_workspace_log_when_workspace_known(tmp_path):
    workspace = tmp_path / "ws"
    notify_debug.emit(str(workspace), "ws.event", a=1)
    log = workspace / "run" / "notify.jsonl"
    record = json.loads(log.read_text().splitlines()[0])
    assert record["event"] == "ws.event"
    assert record["component"] == "notify"
    assert record["workspace"] == str(workspace)
    assert record["a"] == 1


def test_emit_for_window_uses_passed_workspace(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    tmux_lookups: list[str] = []
    monkeypatch.setattr(
        notify_debug,
        "workspace_for_window",
        lambda window_target: tmux_lookups.append(window_target) or "",
    )
    notify_debug.emit_for_window("dev:1", "ui.event", workspace=str(workspace), payload="x")
    log = workspace / "run" / "notify.jsonl"
    record = json.loads(log.read_text().splitlines()[0])
    assert record["event"] == "ui.event"
    assert record["payload"] == "x"
    assert tmux_lookups == []  # passed workspace skips lookup


def test_emit_for_window_resolves_workspace_when_not_passed(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"
    monkeypatch.setattr(notify_debug, "workspace_for_window", lambda _wt: str(workspace))
    notify_debug.emit_for_window("dev:1", "ui.event", payload="resolved")
    log = workspace / "run" / "notify.jsonl"
    record = json.loads(log.read_text().splitlines()[0])
    assert record["event"] == "ui.event"
    assert record["payload"] == "resolved"


def _mock_tmux_basics(monkeypatch):
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_window_name", lambda _pane: "dev")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_window_target", lambda _pane: "dev:1")
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_option", lambda _pane, key: "orch" if key == "hive-agent" else None)
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_session_name", lambda _pane: "dev")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_window", lambda _session: "dev:9")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_tty", lambda _session: "/dev/ttys050")
    monkeypatch.setattr("hive.notify_ui.tmux.set_pane_option", lambda *args, **kwargs: None)


def test_notify_fires_flash_and_bell(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    flash_calls: list[tuple] = []
    bell_calls: list[str] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "terminal")
    monkeypatch.setattr(
        "hive.notify_ui.show_window_flash",
        lambda msg, pane, wt, wn, agent_name="", animate_on_arrival=True, **_kwargs: flash_calls.append(
            (msg, pane, wt, wn, agent_name, animate_on_arrival)
        ),
    )
    monkeypatch.setattr("hive.notify_ui._ring_terminal_bell", lambda pane, **_kwargs: bell_calls.append(pane))

    payload = notify_ui.notify("回来确认", "%9")

    assert payload["surface"] == "fired"
    assert payload["suppressed"] is False
    assert flash_calls == [("回来确认", "%9", "dev:1", "dev", "orch", True)]
    assert bell_calls == ["%9"]


def test_notify_is_silent_when_target_window_is_focused(monkeypatch):
    _mock_tmux_basics(monkeypatch)
    calls: list[tuple] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "terminal")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_window", lambda _session: "dev:1")
    monkeypatch.setattr("hive.notify_ui.show_window_flash", lambda *args, **kwargs: calls.append(("flash",)))
    monkeypatch.setattr("hive.notify_ui._ring_terminal_bell", lambda pane, **_kwargs: calls.append(("bell",)))
    monkeypatch.setattr("hive.notify_ui.tmux.set_pane_option", lambda *args, **kwargs: calls.append(("pane-option",)))
    # debug logging may resolve workspace via tmux on its own; neutralize to keep
    # this test focused on production side effects.
    monkeypatch.setattr("hive.notify_ui.notify_debug.emit_for_window", lambda *args, **kwargs: None)

    payload = notify_ui.notify("回来确认", "%9")

    assert payload["surface"] == "suppressed"
    assert payload["suppressed"] is True
    assert payload["suppressionReason"] == "focused_window"
    assert calls == []


def _mock_show_flash_side_effects(monkeypatch, *, existing_original=None):
    rename_calls: list[tuple] = []
    option_calls: list[tuple] = []
    pane_option_calls: list[tuple] = []
    run_calls: list[tuple] = []
    attention_args: list[dict] = []

    state = {"original": existing_original}

    def fake_get(target, key):
        if key == "hive-notify-original-name":
            return state["original"]
        return None

    def fake_set(target, option, value):
        option_calls.append((target, option, value))
        if option == "@hive-notify-original-name":
            state["original"] = value

    monkeypatch.setattr("hive.notify_ui.tmux.rename_window", lambda wt, name: rename_calls.append((wt, name)))
    monkeypatch.setattr("hive.notify_ui.tmux.get_window_option", fake_get)
    monkeypatch.setattr("hive.notify_ui.tmux.set_window_option", fake_set)
    monkeypatch.setattr("hive.notify_ui.tmux.set_pane_option", lambda pane, key, value: pane_option_calls.append((pane, key, value)))
    monkeypatch.setattr("hive.notify_ui.tmux._run", lambda args, check=False: run_calls.append(args))
    monkeypatch.setattr(
        "hive.notify_ui._write_pane_attention_script",
        lambda **kwargs: attention_args.append(kwargs) or __import__("pathlib").Path("/tmp/hive-pane-attention.sh"),
    )
    return rename_calls, option_calls, pane_option_calls, run_calls, attention_args


def test_show_window_flash_renames_sets_reverse_bold_and_hook(monkeypatch):
    rename_calls, option_calls, pane_option_calls, run_calls, attention_args = _mock_show_flash_side_effects(monkeypatch)

    notify_ui.show_window_flash("Agent finished", "%9", "dev:1", "dev", agent_name="orch")

    assert rename_calls == [("dev:1", "dev · 🤖 orch")]
    token_value = [v for (_, opt, v) in option_calls if opt == "@hive-notify-token"][0]
    assert token_value.startswith("%9:")
    assert pane_option_calls == [("%9", "hive-notify-active", token_value)]
    assert attention_args == [{"pane_id": "%9", "token": token_value}]
    hook_name_value = [v for (_, opt, v) in option_calls if opt == "@hive-notify-hook"][0]
    assert hook_name_value == notify_ui.SELECT_HOOK_NAME
    assert option_calls == [
        ("dev:1", "@hive-notify-original-name", "dev"),
        ("dev:1", "@hive-notify-token", token_value),
        ("dev:1", "@hive-notify-hook", hook_name_value),
        ("dev:1", "@hive-notify-attention", "/tmp/hive-pane-attention.sh"),
        ("dev:1", "window-status-style", "reverse,bold"),
        ("dev:1", "window-status-current-style", "reverse,bold"),
    ]
    assert len(run_calls) == 1
    hook_cmd = run_calls[0]
    assert hook_cmd[0:4] == ["set-hook", "-t", "dev", notify_ui.SELECT_HOOK_NAME]
    assert "set-hook -ut" not in hook_cmd[4]
    assert "/tmp/hive-notify-" not in hook_cmd[4]
    assert "-m hive.notify_ui --cleanup-selected" in hook_cmd[4]
    assert "'#{client_tty}'" in hook_cmd[4]


def test_show_window_flash_can_skip_arrival_animation(monkeypatch):
    rename_calls, option_calls, pane_option_calls, run_calls, attention_args = _mock_show_flash_side_effects(monkeypatch)

    notify_ui.show_window_flash(
        "Agent finished",
        "%9",
        "dev:1",
        "dev",
        agent_name="orch",
        animate_on_arrival=False,
    )

    token_value = [v for (_, opt, v) in option_calls if opt == "@hive-notify-token"][0]
    assert rename_calls == [("dev:1", "dev · 🤖 orch")]
    assert token_value.startswith("%9:")
    assert pane_option_calls == []
    assert attention_args == []
    assert not [item for item in option_calls if item[1] == "@hive-notify-attention"]
    assert len(run_calls) == 1


def test_show_window_flash_without_agent_name_uses_bare_flag(monkeypatch):
    rename_calls, _, _, _, _ = _mock_show_flash_side_effects(monkeypatch)

    notify_ui.show_window_flash("Agent finished", "%9", "dev:1", "dev")

    assert rename_calls == [("dev:1", "dev · 🤖")]


def test_double_notify_preserves_original_and_does_not_rewrite_original_option(monkeypatch):
    rename_calls, option_calls, _, _, _ = _mock_show_flash_side_effects(monkeypatch)

    notify_ui.show_window_flash("m1", "%9", "dev:1", "dev", agent_name="orch")
    notify_ui.show_window_flash("m2", "%9", "dev:1", "dev · 🤖 orch", agent_name="orch")

    assert rename_calls == [
        ("dev:1", "dev · 🤖 orch"),
        ("dev:1", "dev · 🤖 orch"),
    ]
    original_writes = [v for (_, opt, v) in option_calls if opt == "@hive-notify-original-name"]
    assert original_writes == ["dev"]


def test_clear_stale_notify_restores_window_options_and_matching_pane(monkeypatch):
    window_options = {
        "hive-notify-token": "%9:old-fire",
        "hive-notify-original-name": "dev",
        "hive-notify-hook": notify_ui.SELECT_HOOK_NAME,
        "hive-notify-attention": "/tmp/missing-attention.sh",
    }
    pane_options = {
        ("%9", "hive-notify-active"): "%9:old-fire",
        ("%10", "hive-notify-active"): "%10:new-fire",
    }
    actions: list[tuple[str, str, str]] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_window_option", lambda _target, key: window_options.get(key))
    monkeypatch.setattr(
        "hive.notify_ui.tmux.clear_window_option",
        lambda target, option: actions.append(("clear-window", target, option)) or window_options.pop(option.lstrip("@"), None),
    )
    monkeypatch.setattr("hive.notify_ui.tmux.rename_window", lambda target, name: actions.append(("rename-window", target, name)))
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_option", lambda pane, key: pane_options.get((pane, key)))
    monkeypatch.setattr(
        "hive.notify_ui.tmux.clear_pane_option",
        lambda pane, key: actions.append(("clear-pane", pane, key)) or pane_options.pop((pane, key), None),
    )

    notify_ui.clear_stale_notify("dev:1", ["%9", "%10"])

    assert actions == [
        ("clear-window", "dev:1", "window-status-style"),
        ("clear-window", "dev:1", "window-status-current-style"),
        ("rename-window", "dev:1", "dev"),
        ("clear-window", "dev:1", "@hive-notify-token"),
        ("clear-window", "dev:1", "@hive-notify-original-name"),
        ("clear-window", "dev:1", "@hive-notify-hook"),
        ("clear-window", "dev:1", "@hive-notify-attention"),
        ("clear-pane", "%9", "hive-notify-active"),
    ]
    assert window_options == {}
    assert pane_options == {("%10", "hive-notify-active"): "%10:new-fire"}


def test_cleanup_selected_window_clears_current_token_and_runs_attention(monkeypatch):
    window_options = {
        "hive-notify-token": "%9:old-fire",
        "hive-notify-original-name": "dev",
        "hive-notify-attention": "/tmp/hive-pane-attention.sh",
    }
    pane_options = {("%9", "hive-notify-active"): "%9:old-fire"}
    attention_calls: list[tuple[str, str]] = []

    monkeypatch.setattr("hive.notify_ui.tmux.get_window_option", lambda _target, key: window_options.get(key))
    monkeypatch.setattr(
        "hive.notify_ui.tmux.clear_window_option",
        lambda _target, option: window_options.pop(option.lstrip("@"), None),
    )
    monkeypatch.setattr("hive.notify_ui.tmux.rename_window", lambda _target, _name: None)
    monkeypatch.setattr("hive.notify_ui.tmux.list_panes", lambda _target: ["%9"])
    monkeypatch.setattr("hive.notify_ui.tmux.get_pane_option", lambda pane, key: pane_options.get((pane, key)))
    monkeypatch.setattr(
        "hive.notify_ui.tmux.clear_pane_option",
        lambda pane, key: pane_options.pop((pane, key), None),
    )
    monkeypatch.setattr(
        "hive.notify_ui._run_attention_script",
        lambda path, client, **_kwargs: attention_calls.append((path, client)),
    )

    assert notify_ui.cleanup_selected_window("dev:1", client="/dev/ttys050") is True

    assert window_options == {}
    assert pane_options == {}
    assert attention_calls == [("/tmp/hive-pane-attention.sh", "/dev/ttys050")]


def test_notify_with_workspace_writes_ui_events(tmp_path, monkeypatch):
    workspace = tmp_path / "ws"

    _mock_tmux_basics(monkeypatch)
    monkeypatch.setattr("hive.notify_ui.tmux.get_client_mode", lambda _pane: "terminal")
    monkeypatch.setattr("hive.notify_ui.tmux.get_most_recent_client_window", lambda _session: "dev:9")
    monkeypatch.setattr("hive.notify_ui.show_window_flash", lambda *args, **kwargs: None)
    monkeypatch.setattr("hive.notify_ui._ring_terminal_bell", lambda *args, **kwargs: None)

    notify_ui.notify("回来确认", "%9", workspace=str(workspace))

    log = workspace / "run" / "notify.jsonl"
    events = [json.loads(line)["event"] for line in log.read_text().splitlines()]
    assert "notify.call" in events


def test_pane_attention_popup_covers_target_pane():
    assert "popup_w = width" in notify_ui._PANE_ATTENTION_PYTHON
    assert "popup_h = height" in notify_ui._PANE_ATTENTION_PYTHON
    assert 'x = "#{popup_pane_left}"' in notify_ui._PANE_ATTENTION_PYTHON
    assert 'y = "#{popup_pane_top}"' in notify_ui._PANE_ATTENTION_PYTHON
    assert "TARGET LOCKED:" in notify_ui._PANE_ATTENTION_PYTHON


def test_pane_attention_animation_timing_is_fast():
    assert "SCAN_FRAMES = 14" in notify_ui._PANE_ATTENTION_PYTHON
    assert "SCAN_DELAY = 0.032" in notify_ui._PANE_ATTENTION_PYTHON
    assert "PULSE_FRAMES = 4" in notify_ui._PANE_ATTENTION_PYTHON
    assert "PULSE_DELAY = 0.055" in notify_ui._PANE_ATTENTION_PYTHON
    script = notify_ui._write_pane_attention_script(pane_id="%9", token="tok")
    try:
        assert "sleep 0.18" in script.read_text()
    finally:
        script.unlink(missing_ok=True)


def test_pane_attention_script_executes_via_facade(tmp_path):
    # the embedded script now imports hive.tmux, so it must run end-to-end
    # under an interpreter that has the package (sys.executable contract)
    import os
    import subprocess
    import sys

    repo_src = Path(__file__).resolve().parents[2] / "src"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    log = tmp_path / "tmux.log"
    fake = bin_dir / "tmux"
    fake.write_text(
        "#!/bin/sh\n"
        f"echo \"$@\" >> {log}\n"
        "case \"$*\" in\n"
        "  *pane_left*) echo '1 2 40 20' ;;\n"
        "  *) echo 'stub' ;;\n"
        "esac\n")
    fake.chmod(0o755)

    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}:{env['PATH']}"
    env["PYTHONPATH"] = str(repo_src)
    env["HIVE_NOTIFY_PANE"] = "%7"
    env["HIVE_NOTIFY_CLIENT"] = ""

    result = subprocess.run(
        [sys.executable, "-c", notify_ui._PANE_ATTENTION_PYTHON],
        env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, result.stderr[-300:]
    entries = log.read_text().splitlines()
    popup = [e for e in entries if e.startswith("display-popup")]
    assert len(popup) == 1
    assert " -B " in popup[0] and " -E " in popup[0]
    assert "PYPOPUP" in popup[0]  # inner animation heredoc still delivered
