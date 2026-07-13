import json
import re
import tomllib

import pytest

from hive import core_hooks

CODEX_HOOK = {"Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}]}


def _merge_with_config(configure_hive_home, pre: str | None):
    hive_home = configure_hive_home()
    config_path = hive_home.parent / ".codex" / "config.toml"
    if pre is not None:
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(pre)
    core_hooks.merge_hook_groups(CODEX_HOOK)
    return config_path


def _features_body(text: str) -> str:
    lines = text.splitlines()
    span = core_hooks._features_span([f"{line}\n" for line in lines])
    assert span is not None
    return "\n".join(lines[span[0]:span[1]])


def _hooks_assignment_count(section_body: str) -> int:
    return len([l for l in section_body.splitlines() if re.match(r"\s*hooks\s*=", l)])


def test_codex_features_flag_written_for_fresh_home(configure_hive_home):
    config_path = _merge_with_config(configure_hive_home, pre=None)
    text = config_path.read_text()
    assert tomllib.loads(text)["features"]["hooks"] is True
    assert "codex_hooks" not in text
    assert _hooks_assignment_count(_features_body(text)) == 1


def test_codex_features_flag_migrates_legacy_and_keeps_user_lines(configure_hive_home):
    pre = '[features]\ncodex_hooks = true\nweb_search = "on"\n\n[tui]\ntheme = "dark"\n'
    config_path = _merge_with_config(configure_hive_home, pre)
    text = config_path.read_text()
    data = tomllib.loads(text)
    assert data["features"]["hooks"] is True
    assert "codex_hooks" not in data["features"]
    assert data["features"]["web_search"] == "on"
    assert data["tui"]["theme"] == "dark"
    assert _hooks_assignment_count(_features_body(text)) == 1


@pytest.mark.parametrize("value", ["true", "false"])
def test_codex_features_flag_respects_explicit_value(configure_hive_home, value):
    pre = f"[features]\nhooks = {value}\n"
    config_path = _merge_with_config(configure_hive_home, pre)
    assert config_path.read_text() == pre


def test_codex_features_flag_migration_keeps_explicit_false(configure_hive_home):
    pre = "[features]\nhooks = false\ncodex_hooks = true\n"
    config_path = _merge_with_config(configure_hive_home, pre)
    text = config_path.read_text()
    assert tomllib.loads(text)["features"] == {"hooks": False}
    assert _hooks_assignment_count(_features_body(text)) == 1


def test_codex_features_flag_ignores_suffix_and_other_sections(configure_hive_home):
    # `hooks = true` is a suffix of `codex_hooks = true`, and other sections
    # may legitimately own a `hooks` key: neither may satisfy or be edited
    pre = "[features]\ncodex_hooks = true\n\n[mcp_servers.x]\nhooks = false\n"
    config_path = _merge_with_config(configure_hive_home, pre)
    data = tomllib.loads(config_path.read_text())
    assert data["features"] == {"hooks": True}
    assert data["mcp_servers"]["x"]["hooks"] is False


def test_codex_features_flag_ignores_commented_assignments(configure_hive_home):
    pre = "[features]\n# codex_hooks = true\n# hooks = false\n"
    config_path = _merge_with_config(configure_hive_home, pre)
    text = config_path.read_text()
    assert tomllib.loads(text)["features"] == {"hooks": True}
    assert "# codex_hooks = true" in text
    assert "# hooks = false" in text


def test_codex_features_flag_appends_section_preserving_layout(configure_hive_home):
    # insertion-only migration: existing bytes survive verbatim, including
    # trailing blank lines the user left on purpose
    pre = '[tui]\ntheme = "dark"\n\n# keep trailing layout\n\n'
    config_path = _merge_with_config(configure_hive_home, pre)
    text = config_path.read_text()
    assert text == pre + "[features]\nhooks = true\n"
    assert tomllib.loads(text)["features"]["hooks"] is True


def test_codex_features_flag_converges_idempotently(configure_hive_home):
    pre = '[features]\ncodex_hooks = true\nweb_search = "on"\n'
    config_path = _merge_with_config(configure_hive_home, pre)
    first = config_path.read_text()
    core_hooks.merge_hook_groups(CODEX_HOOK)
    assert config_path.read_text() == first


def test_merge_and_remove_hook_groups_round_trip(configure_hive_home):
    hive_home = configure_hive_home()
    claude_home = hive_home.parent / ".claude"
    codex_home = hive_home.parent / ".codex"
    hook_defs = {
        "Notification": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "/tmp/notify-hook", "timeout": 5}]}],
    }

    core_hooks.merge_hook_groups(hook_defs)

    claude_settings = json.loads((claude_home / "settings.json").read_text())
    codex_hooks = json.loads((codex_home / "hooks.json").read_text())

    assert claude_settings["hooks"]["Notification"] == hook_defs["Notification"]
    assert claude_settings["hooks"]["Stop"] == hook_defs["Stop"]
    assert codex_hooks["hooks"]["Stop"] == hook_defs["Stop"]
    assert "Notification" not in codex_hooks["hooks"]

    core_hooks.remove_hook_groups(hook_defs)

    assert "hooks" not in json.loads((claude_home / "settings.json").read_text())
    assert "hooks" not in json.loads((codex_home / "hooks.json").read_text())


def test_merge_hook_groups_preserves_unmanaged_entries(configure_hive_home):
    hive_home = configure_hive_home()
    claude_home = hive_home.parent / ".claude"
    settings_path = claude_home / "settings.json"
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
