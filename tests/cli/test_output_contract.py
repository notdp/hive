"""Structural + behavioral contract tests for the CLI consistency migration.

Covers: --version (A1), section-mapping completeness (A4), the JSON-default
output contract on the 11 migrated commands (B1), -h help-option scope and
launcher/tombstone exemptions (B3), and the ls aliases (B5).
"""

import json

import click

from hive.cli import _COMMAND_HELP_SECTIONS, _hive_version, cli

# The exact command set migrated to the JSON-default contract (B1), plus the
# `plugin ls` alias which mirrors `plugin list`.
JSON_DEFAULT_PATHS = {
    "ls",
    "worktree start",
    "worktree done",
    "worktree status",
    "plugin list",
    "plugin ls",
    "plugin enable",
    "plugin disable",
    "duo set-pr",
    "duo clear-pr",
    "config roles",
    "squad set-integration-branch",
}


def _walk(cmd, prefix=()):
    if prefix:
        yield " ".join(prefix), cmd
    if isinstance(cmd, click.Group):
        for name, child in cmd.commands.items():
            yield from _walk(child, prefix + (name,))


def _option(cmd, flag):
    for p in cmd.params:
        if isinstance(p, click.Option) and flag in p.opts:
            return p
    return None


# --- A1: --version ---


def test_version_flag_reports_version(runner):
    result = runner.invoke(cli, ["--version"])
    assert result.exit_code == 0, result.output
    version = _hive_version()
    assert version and version != "unknown"
    assert version in result.output


# --- A4: every visible root command has an explicit help section ---


def test_every_visible_root_command_is_section_mapped():
    unmapped = [
        name
        for name, cmd in cli.commands.items()
        if not cmd.hidden and name not in _COMMAND_HELP_SECTIONS
    ]
    assert unmapped == [], f"visible commands silently falling into 'Other Commands': {unmapped}"


# --- B1: JSON-default output contract ---


def test_json_default_commands_carry_plain_and_hidden_legacy_json():
    tree = dict(_walk(cli))
    for path in sorted(JSON_DEFAULT_PATHS):
        cmd = tree[path]
        plain = _option(cmd, "--plain")
        legacy = _option(cmd, "--json")
        assert plain is not None and not plain.hidden, f"{path}: visible --plain missing"
        assert legacy is not None and legacy.hidden, f"{path}: hidden compat --json missing"
        assert legacy.expose_value is False, f"{path}: --json must be a no-op (expose_value=False)"


def test_no_other_command_gained_output_flags():
    tree = dict(_walk(cli))
    unexpected = [
        path
        for path, cmd in tree.items()
        if not isinstance(cmd, click.Group)
        and _option(cmd, "--plain") is not None
        and path not in JSON_DEFAULT_PATHS
    ]
    assert unexpected == [], f"--plain leaked onto commands outside the migration set: {unexpected}"


def test_plugin_list_default_json_equals_legacy_json_and_plain_wins(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    default = runner.invoke(cli, ["plugin", "list"])
    legacy = runner.invoke(cli, ["plugin", "list", "--json"])
    both = runner.invoke(cli, ["plugin", "list", "--plain", "--json"])
    plain = runner.invoke(cli, ["plugin", "list", "--plain"])

    assert default.exit_code == legacy.exit_code == both.exit_code == plain.exit_code == 0
    assert json.loads(default.output) == json.loads(legacy.output)
    assert both.output == plain.output  # --plain wins over the legacy no-op
    assert "Plugins (" in plain.output  # human rendering, not JSON


def test_config_roles_default_is_json_and_never_interactive(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    # A mocked TTY on both ends must NOT open the picker on the default path.
    monkeypatch.setattr("sys.stdin.isatty", lambda: True)
    monkeypatch.setattr("sys.stdout.isatty", lambda: True)
    monkeypatch.setattr(
        "hive.cli._term_menu",
        lambda *_a, **_k: (_ for _ in ()).throw(AssertionError("picker opened on default path")),
    )
    result = runner.invoke(cli, ["config", "roles"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert set(payload) >= {"worker", "validator"}


def test_config_roles_plain_without_tty_fails_without_mutation(runner, configure_hive_home, tmp_path):
    hive_home = configure_hive_home(tmux_inside=False)
    settings = hive_home / "settings.json"
    before = settings.read_text() if settings.exists() else None
    result = runner.invoke(cli, ["config", "roles", "--plain"])
    assert result.exit_code == 1
    assert "terminal" in result.output
    after = settings.read_text() if settings.exists() else None
    assert before == after


# --- B3: -h scope and exemptions ---


def test_root_registers_dash_h():
    assert cli.context_settings.get("help_option_names") == ["-h", "--help"]


def test_dash_h_shows_help_on_strict_commands(runner):
    for argv in (["-h"], ["send", "-h"], ["team", "-h"], ["duo", "-h"]):
        result = runner.invoke(cli, argv)
        assert result.exit_code == 0, (argv, result.output)
        assert "Usage:" in result.output


def test_ignore_unknown_commands_pin_help_to_double_dash_only():
    """Guard: every raw/tombstone command must explicitly keep --help-only so
    the root -h registration can never silently change its forwarding."""
    for path, cmd in _walk(cli):
        settings = cmd.context_settings or {}
        if settings.get("ignore_unknown_options"):
            assert settings.get("help_option_names") == ["--help"], (
                f"{path}: ignore_unknown_options command must pin help_option_names=['--help']"
            )


def test_launchers_keep_no_help_option():
    for name in ("claude", "codex", "grok"):
        assert cli.commands[name].add_help_option is False, name


def test_dash_h_still_lands_in_raw_args_for_launchers():
    """-h is forwarded, not intercepted, on the exempted paths (parser-level
    probe via make_context; callbacks are never invoked)."""
    codex_ctx = cli.commands["codex"].make_context("codex", ["-h"])
    assert codex_ctx.args == ["-h"]
    grok_ctx = cli.commands["grok"].make_context("grok", ["-h"])
    assert grok_ctx.args == ["-h"]
    cvim_ctx = cli.commands["cvim"].make_context("cvim", ["-h"])
    assert cvim_ctx.params.get("args") == ("-h",)
    claude_ctx = cli.commands["claude"].make_context("claude", ["--help"])
    assert claude_ctx.args == ["--help"]


def test_dash_h_still_reaches_status_tombstone(runner):
    result = runner.invoke(cli, ["status", "-h"])
    assert result.exit_code == 1  # removal shim, not a help page
    assert "was removed" in result.output


# --- B5: ls aliases ---


def test_skills_ls_matches_skills_list(runner):
    ls = runner.invoke(cli, ["skills", "ls"])
    lst = runner.invoke(cli, ["skills", "list"])
    assert ls.exit_code == lst.exit_code == 0
    assert json.loads(ls.output) == json.loads(lst.output)


def test_plugin_ls_matches_plugin_list_in_all_modes(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    for extra in ([], ["--json"], ["--plain"]):
        ls = runner.invoke(cli, ["plugin", "ls", *extra])
        lst = runner.invoke(cli, ["plugin", "list", *extra])
        assert ls.exit_code == lst.exit_code == 0, (extra, ls.output, lst.output)
        assert ls.output == lst.output, extra


def test_ls_aliases_are_hidden():
    assert cli.commands["skills"].commands["ls"].hidden
    assert cli.commands["plugin"].commands["ls"].hidden
