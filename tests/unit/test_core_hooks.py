import json
from pathlib import Path

from hive import core_hooks


def test_ensure_session_locator_hook_installed_is_idempotent(configure_hive_home):
    hive_home = configure_hive_home()
    factory_home = hive_home.parent / ".factory"

    first = core_hooks.ensure_session_locator_hook_installed()
    second = core_hooks.ensure_session_locator_hook_installed()

    script_path = Path(first["script"])
    settings = json.loads((factory_home / "settings.json").read_text())

    assert script_path.exists()
    assert script_path == Path(second["script"])
    assert first["sessionMap"].endswith("/hive/session-map.json")
    for event in ("SessionStart", "UserPromptSubmit", "SessionEnd"):
        groups = settings["hooks"][event]
        assert len(groups) == 1
        assert groups[0]["hooks"][0]["command"] == str(script_path)


def test_ensure_session_locator_hook_preserves_unmanaged_hook_entries(configure_hive_home):
    hive_home = configure_hive_home()
    factory_home = hive_home.parent / ".factory"
    settings_path = factory_home / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text(json.dumps({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/droid-session-map-hook"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "/tmp/custom-hook"}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": str(hive_home / "core" / "bin" / "droid-session-map-hook")}]}],
        }
    }))

    result = core_hooks.ensure_session_locator_hook_installed()
    settings = json.loads(settings_path.read_text())
    expected_hook = result["script"]

    assert result["hooksRemoved"] == 1
    assert result["hooksInstalled"] == 3
    assert settings["hooks"]["SessionStart"][0]["hooks"][0]["command"] == "~/.dotfiles/bin/droid-session-map-hook"
    assert settings["hooks"]["SessionStart"][1]["hooks"][0]["command"] == expected_hook
    assert settings["hooks"]["SessionEnd"] == [{"hooks": [{"type": "command", "command": expected_hook}]}]
    assert settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"] == "/tmp/custom-hook"
    assert settings["hooks"]["UserPromptSubmit"][1]["hooks"][0]["command"] == expected_hook


def test_resolve_session_record_prefers_pane_then_pid_then_tty(configure_hive_home):
    configure_hive_home()
    session_map = core_hooks.session_map_path()
    session_map.parent.mkdir(parents=True, exist_ok=True)
    session_map.write_text(json.dumps({
        "by_pane": {"%9": {"session_id": "pane-sess", "transcript_path": "/tmp/pane.jsonl"}},
        "by_pid": {"1234": {"session_id": "pid-sess", "transcript_path": "/tmp/pid.jsonl"}},
        "by_tty": {"/dev/ttys009": {"session_id": "tty-sess", "transcript_path": "/tmp/tty.jsonl"}},
    }))

    assert core_hooks.resolve_session_record(pane_id="%9", pid="1234", tty="/dev/ttys009") == {
        "session_id": "pane-sess",
        "transcript_path": "/tmp/pane.jsonl",
    }
    assert core_hooks.resolve_session_record(pid="1234", tty="/dev/ttys009") == {
        "session_id": "pid-sess",
        "transcript_path": "/tmp/pid.jsonl",
    }
    assert core_hooks.resolve_session_record(tty="/dev/ttys009") == {
        "session_id": "tty-sess",
        "transcript_path": "/tmp/tty.jsonl",
    }



