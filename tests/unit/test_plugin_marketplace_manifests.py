"""Structure tests for the published hive-plugins marketplace and manifests.

These lock the executable contract of the plugin distribution foundation:
no machine-specific paths, dual-manifest version consistency, and the
plugin-owned canonical skill.
"""
import json
import re
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

REPO = Path(__file__).resolve().parents[2]

CC_MARKETPLACE = REPO / ".claude-plugin" / "marketplace.json"
CODEX_MARKETPLACE = REPO / ".agents" / "plugins" / "marketplace.json"
HIVE_CC = REPO / "plugins" / "hive" / ".claude-plugin" / "plugin.json"
HIVE_CODEX = REPO / "plugins" / "hive" / ".codex-plugin" / "plugin.json"
ALL_MANIFESTS = [CC_MARKETPLACE, CODEX_MARKETPLACE, HIVE_CC, HIVE_CODEX]

CANONICAL_SKILL = REPO / "plugins" / "hive" / "skills" / "hive" / "SKILL.md"

SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def _load(path: Path) -> dict:
    return json.loads(path.read_text())


def test_all_manifests_parse():
    for path in ALL_MANIFESTS:
        assert isinstance(_load(path), dict), path


def test_both_marketplaces_declare_the_same_name():
    # the marketplace name is half of every plugin ref (hive@hive), so the two
    # published files must agree; the runtime half of this check is gone with
    # claude_channel.MARKETPLACE_NAME -- no src constant names it any more
    for path in (CC_MARKETPLACE, CODEX_MARKETPLACE):
        assert _load(path)["name"] == "hive", path


def test_cc_marketplace_lists_only_hive_with_a_real_source():
    entries = {p["name"]: p for p in _load(CC_MARKETPLACE)["plugins"]}
    assert set(entries) == {"hive"}
    for entry in entries.values():
        source = entry["source"]
        assert source.startswith("./"), entry
        assert (REPO / source).is_dir(), entry
    # claude delivery moved to Claude Code's own cross-session inbox: the
    # channel plugin is deleted, not merely unlisted
    assert not (REPO / "plugins" / "hive-channel").exists()


def test_codex_marketplace_lists_only_hive():
    data = _load(CODEX_MARKETPLACE)
    entries = data["plugins"]
    assert [p["name"] for p in entries] == ["hive"]
    entry = entries[0]
    assert entry["source"] == {"source": "local", "path": "./plugins/hive"}
    assert (REPO / entry["source"]["path"]).is_dir()
    # codex ingestion enums are strict: unknown variants reject the whole
    # marketplace file (validator r1: "NONE" failed `codex plugin marketplace add`)
    assert entry["policy"]["installation"] == "AVAILABLE"
    assert entry["policy"]["authentication"] == "ON_INSTALL"
    assert entry["policy"]["authentication"] in {"ON_INSTALL", "ON_USE"}
    assert "category" in entry
    # a Claude-only key must not leak into the codex marketplace (codex
    # ingestion is strict); the hive-channel guard itself is retired:
    assert "channels" not in CODEX_MARKETPLACE.read_text()
    # the CC-only channel plugin they kept out of codex ingestion no longer
    # exists on either side


def test_no_machine_paths_in_any_manifest():
    forbidden = ["/Users/", "/private/", "PYTHONPATH", "HIVE_HOME", str(REPO)]
    for path in ALL_MANIFESTS:
        raw = path.read_text()
        for needle in forbidden:
            assert needle not in raw, f"{path}: {needle}"


def test_plugin_versions_track_the_cli_version():
    # single version concept (human directive): every published plugin
    # manifest carries the pyproject version, so a CLI bump ships plugins too
    import tomllib

    cli_version = tomllib.loads((REPO / "pyproject.toml").read_text())["project"]["version"]
    assert SEMVER.match(cli_version), cli_version
    for manifest in (HIVE_CC, HIVE_CODEX):
        assert _load(manifest)["version"] == cli_version, manifest


def test_canonical_skill_is_the_only_shipped_copy():
    assert CANONICAL_SKILL.is_file() and not CANONICAL_SKILL.is_symlink()
    # the npx-era copies are retired: the plugin owns the single source
    assert not (REPO / "skills").exists()
    assert not (REPO / "src" / "hive" / "core_assets" / "skills").exists()


# --- bootstrap hook (invisible-update contract) ------------------------------

HOOKS_FILE = REPO / "plugins" / "hive" / "hooks" / "hooks.json"
CODEX_HOOKS_FILE = REPO / "plugins" / "hive" / "hooks" / "codex-hooks.json"


def test_hooks_declarations_are_asymmetric_by_design():
    # Claude auto-loads hooks/hooks.json (an explicit manifest entry makes it
    # load twice: "Duplicate hooks file detected"). Codex would auto-load the
    # same file and skip the whole hook over its `async` key ("async hooks are
    # not supported yet"), so its manifest must override the default with the
    # async-free variant.
    assert "hooks" not in _load(HIVE_CC)
    rel = _load(HIVE_CODEX)["hooks"]
    assert rel == "./hooks/codex-hooks.json"
    target = (HIVE_CODEX.parent.parent / rel).resolve()
    assert target == CODEX_HOOKS_FILE.resolve()
    assert target.is_file()
    assert target.is_relative_to((REPO / "plugins" / "hive").resolve())


def test_codex_hook_is_the_claude_hook_without_async():
    # lockstep contract: strip Claude's `async` and Codex's stdout redirect;
    # everything else must stay deep-equal so a command/timeout edit on one
    # side cannot ship without the other
    claude = _load(HOOKS_FILE)
    codex = _load(CODEX_HOOKS_FILE)
    for group in claude["hooks"]["SessionStart"]:
        for hook in group["hooks"]:
            hook.pop("async", None)
    for group in codex["hooks"]["SessionStart"]:
        for hook in group["hooks"]:
            hook["command"] = hook["command"].removesuffix(" >/dev/null")
    assert claude == codex


def test_codex_hook_redirects_stdout_only():
    # Codex feeds SessionStart stdout into developer context, so the success
    # summaries must be dropped; stderr stays for the single-line remediation
    hooks = [h for g in _load(CODEX_HOOKS_FILE)["hooks"]["SessionStart"] for h in g["hooks"]]
    assert len(hooks) == 1
    command = hooks[0]["command"]
    assert command.endswith(" >/dev/null")
    assert "${CLAUDE_PLUGIN_ROOT}/scripts/bootstrap.py" in command
    assert "2>" not in command
    assert "&" not in command
    assert "async" not in CODEX_HOOKS_FILE.read_text()
    assert _load(HOOKS_FILE)["hooks"]["SessionStart"][0]["hooks"][0]["async"] is True


def test_bootstrap_hook_runs_script_via_plugin_root():
    for hooks_file in (HOOKS_FILE, CODEX_HOOKS_FILE):
        data = _load(hooks_file)
        groups = data["hooks"]["SessionStart"]
        commands = [h["command"] for g in groups for h in g["hooks"]]
        assert any("${CLAUDE_PLUGIN_ROOT}/scripts/bootstrap.py" in c for c in commands)
        raw = hooks_file.read_text()
        for needle in ["/Users/", "/private/", str(REPO)]:
            assert needle not in raw, needle
    assert (REPO / "plugins" / "hive" / "scripts" / "bootstrap.py").is_file()
