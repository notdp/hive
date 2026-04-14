import json

from hive import core_hooks


def test_merge_and_remove_hook_groups_round_trip(configure_hive_home):
    hive_home = configure_hive_home()
    factory_home = hive_home.parent / ".factory"
    claude_home = hive_home.parent / ".claude"
    codex_home = hive_home.parent / ".codex"
    hook_defs = {
        "Notification": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
    }

    core_hooks.merge_hook_groups(hook_defs)

    factory_settings = json.loads((factory_home / "settings.json").read_text())
    claude_settings = json.loads((claude_home / "settings.json").read_text())
    codex_hooks = json.loads((codex_home / "hooks.json").read_text())

    assert factory_settings["hooks"]["Notification"] == hook_defs["Notification"]
    assert factory_settings["hooks"]["Stop"] == hook_defs["Stop"]
    assert claude_settings["hooks"]["Notification"] == hook_defs["Notification"]
    assert claude_settings["hooks"]["Stop"] == hook_defs["Stop"]
    assert codex_hooks["hooks"]["Stop"] == hook_defs["Stop"]
    assert "Notification" not in codex_hooks["hooks"]

    core_hooks.remove_hook_groups(hook_defs)

    assert "hooks" not in json.loads((factory_home / "settings.json").read_text())
    assert "hooks" not in json.loads((claude_home / "settings.json").read_text())
    assert "hooks" not in json.loads((codex_home / "hooks.json").read_text())


def test_merge_hook_groups_preserves_unmanaged_entries(configure_hive_home):
    hive_home = configure_hive_home()
    factory_home = hive_home.parent / ".factory"
    settings_path = factory_home / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text(json.dumps({
        "hooks": {
            "Notification": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/notify-hook"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/custom-hook"}]}],
        }
    }))
    hook_defs = {
        "Notification": [{"hooks": [{"type": "command", "command": "/tmp/hive-notify-hook", "timeout": 5}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "/tmp/hive-notify-hook", "timeout": 5}]}],
    }

    core_hooks.merge_hook_groups(hook_defs)
    settings = json.loads(settings_path.read_text())

    assert settings["hooks"]["Notification"][0]["hooks"][0]["command"] == "~/.dotfiles/bin/notify-hook"
    assert settings["hooks"]["Notification"][1]["hooks"][0]["command"] == "/tmp/hive-notify-hook"
    assert settings["hooks"]["Stop"][0]["hooks"][0]["command"] == "/tmp/custom-hook"
    assert settings["hooks"]["Stop"][1]["hooks"][0]["command"] == "/tmp/hive-notify-hook"
