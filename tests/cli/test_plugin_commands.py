import json

from hive.cli import cli


def test_plugin_list_enable_and_disable_cvim(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"
    (factory_home / "settings.json").parent.mkdir(parents=True, exist_ok=True)
    (factory_home / "settings.json").write_text(json.dumps({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/droid-session-map-hook"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/droid-session-map-hook"}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": "~/.dotfiles/bin/droid-session-map-hook"}]}],
        }
    }))

    listed = runner.invoke(cli, ["plugin", "list"])
    assert listed.exit_code == 0
    assert "Plugins (0/4 enabled)" in listed.output
    assert "cvim" in listed.output
    assert "disabled" in listed.output

    settings = json.loads((factory_home / "settings.json").read_text())
    expected_hook = str(hive_home / "core" / "bin" / "droid-session-map-hook")
    for event in ("SessionStart", "UserPromptSubmit", "SessionEnd"):
        assert settings["hooks"][event][0]["hooks"][0]["command"] == "~/.dotfiles/bin/droid-session-map-hook"
        assert settings["hooks"][event][1]["hooks"][0]["command"] == expected_hook

    listed_json = runner.invoke(cli, ["plugin", "list", "--json"])
    assert listed_json.exit_code == 0
    names = {item["name"]: item for item in json.loads(listed_json.output)}
    assert {"cvim", "notify", "code-review", "fork"}.issubset(names)
    assert names["cvim"]["enabled"] is False

    enabled = runner.invoke(cli, ["plugin", "enable", "cvim"])
    assert enabled.exit_code == 0
    assert "Plugin 'cvim' enabled." in enabled.output
    assert "commands: cvim, vim" in enabled.output

    enabled_json = runner.invoke(cli, ["plugin", "enable", "cvim", "--json"])
    assert enabled_json.exit_code == 0
    enable_payload = json.loads(enabled_json.output)
    assert enable_payload["enabled"] is True
    assert (factory_home / "commands" / "vim").exists()
    assert not (factory_home / "commands" / "vim").is_symlink()
    assert (factory_home / "commands" / "cvim").exists()
    assert not (factory_home / "commands" / "cvim").is_symlink()
    install_root = hive_home / "plugins" / "installed" / "cvim"
    installed_runner = install_root / "bin" / "droid-vim-command"
    installed_payload_builder = install_root / "bin" / "droid-vim-payload"
    installed_seed_helper = install_root / "bin" / "droid-vim-seed"
    installed_session_helper = install_root / "bin" / "droid-vim-session"
    installed_protocol = install_root / "resources" / "droid_edit_protocol.json"
    assert installed_runner.exists()
    assert installed_payload_builder.exists()
    assert installed_seed_helper.exists()
    assert installed_session_helper.exists()
    assert installed_protocol.exists()
    installed_runner_text = installed_runner.read_text()
    installed_cvim_text = (factory_home / "commands" / "cvim").read_text()
    installed_vim_text = (factory_home / "commands" / "vim").read_text()
    assert "from hive." not in installed_runner_text
    assert 'src_pane_pid="$(tmux display-message -p -t "$src_pane" \'#{pane_pid}\')"' in installed_runner_text
    assert 'src_pane_tty="$(tmux display-message -p -t "$src_pane" \'#{pane_tty}\')"' in installed_runner_text
    assert 'src_pane_tty_key="${src_pane_tty#/dev/}"' in installed_runner_text
    assert 'seed_helper="$script_dir/droid-vim-seed"' in installed_runner_text
    assert 'session_helper="$script_dir/droid-vim-session"' in installed_runner_text
    assert '"$seed_helper" "$cwd" "$dst" "$session_map_file" "$droid_pid" "$droid_tty" "$droid_args"' in installed_runner_text
    assert 'transcript_path="$("$session_helper" "$session_map_file" "$src_cwd" "" "$src_pane_tty_key" "")"' in installed_runner_text
    assert '"$seed_helper" "$src_cwd" "$msg_file" "$transcript_path"' in installed_runner_text
    assert 'capture_session_seed_to_file "$src_cwd" "$msg_file" "$droid_pid" "$droid_tty" "$droid_args"' in installed_runner_text
    assert '"$helper_file" "$reply_pane" "$editor_bin" "$msg_file" "$orig_file" "$tmpdir" "$output_mode"' in installed_runner_text
    assert '"$submit_delay_default" "$cursor_mode" "$payload_builder"' in installed_runner_text
    assert 'payload_builder="${13}"' in installed_runner_text
    assert "\"$payload_builder\" \"$orig_file\" \"$msg_file\" \"$send_file\" \"$send_mode\"" in installed_runner_text
    payload_builder_text = installed_payload_builder.read_text()
    assert "droid_edit_protocol.json" in payload_builder_text
    assert "<edit_target>" in payload_builder_text
    protocol_text = installed_protocol.read_text()
    assert "紧邻上一条 assistant message" in protocol_text
    assert "previous_assistant_message" in protocol_text
    assert str(installed_runner) in installed_cvim_text
    assert str(installed_runner) in installed_vim_text
    assert "previous_assistant_message</edit_target>" in installed_cvim_text
    assert "immediately" in installed_cvim_text
    assert "previous_assistant_message</edit_target>" in installed_vim_text
    assert "immediately" in installed_vim_text

    settings = json.loads((factory_home / "settings.json").read_text())
    for event in ("SessionStart", "UserPromptSubmit", "SessionEnd"):
        assert settings["hooks"][event][0]["hooks"][0]["command"] == "~/.dotfiles/bin/droid-session-map-hook"
        assert settings["hooks"][event][1]["hooks"][0]["command"] == expected_hook

    relisted = runner.invoke(cli, ["plugin", "list", "--json"])
    assert relisted.exit_code == 0
    relisted_payload = {item["name"]: item for item in json.loads(relisted.output)}
    assert relisted_payload["cvim"]["enabled"] is True

    disabled = runner.invoke(cli, ["plugin", "disable", "cvim"])
    assert disabled.exit_code == 0
    assert "Plugin 'cvim' disabled." in disabled.output
    assert not (factory_home / "commands" / "vim").exists()
    assert not (factory_home / "commands" / "cvim").exists()
    assert (hive_home / "core" / "bin" / "droid-session-map-hook").exists()


def test_plugin_enable_cross_review_materializes_skill(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"

    enabled = runner.invoke(cli, ["plugin", "enable", "code-review"])

    assert enabled.exit_code == 0
    assert "Plugin 'code-review' enabled." in enabled.output
    assert "skills: code-review" in enabled.output
    assert (factory_home / "skills" / "code-review").is_symlink()
    assert (hive_home / "plugins" / "installed" / "code-review" / "skills" / "code-review" / "SKILL.md").exists()

    enabled_json = runner.invoke(cli, ["plugin", "enable", "code-review", "--json"])
    assert enabled_json.exit_code == 0
    assert json.loads(enabled_json.output)["enabled"] is True


def test_plugin_enable_notify_materializes_command_and_hooks(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    factory_home = hive_home.parent / ".factory"

    enabled = runner.invoke(cli, ["plugin", "enable", "notify"])

    assert enabled.exit_code == 0
    assert "Plugin 'notify' enabled." in enabled.output
    assert "commands: notify" in enabled.output
    assert (factory_home / "commands" / "notify").exists()

    installed_root = hive_home / "plugins" / "installed" / "notify"
    hook_runner = installed_root / "bin" / "droid-notify-hook"
    hook_defs = installed_root / "hooks" / "hooks.json"
    assert hook_runner.exists()
    assert hook_defs.exists()

    settings = json.loads((factory_home / "settings.json").read_text())
    for event in ("Notification", "Stop"):
        group = settings["hooks"][event][0]
        assert group["hooks"][0]["command"] == str(hook_runner)
        assert group["hooks"][0]["timeout"] == 5
