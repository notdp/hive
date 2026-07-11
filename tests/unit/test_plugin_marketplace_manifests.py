"""Structure tests for the published hive-plugins marketplace and manifests.

These lock the executable contract of the plugin distribution foundation:
portable channel command, no machine-specific paths, dual-manifest version
consistency, and the canonical-skill symlink shape that skill_sync and wheel
packaging depend on.
"""
import json
import re
from pathlib import Path

import pytest

from hive import skill_sync
from hive.adapters import claude_channel

pytestmark = pytest.mark.unit

REPO = Path(__file__).resolve().parents[2]

CC_MARKETPLACE = REPO / ".claude-plugin" / "marketplace.json"
CODEX_MARKETPLACE = REPO / ".agents" / "plugins" / "marketplace.json"
HIVE_CC = REPO / "plugins" / "hive" / ".claude-plugin" / "plugin.json"
HIVE_CODEX = REPO / "plugins" / "hive" / ".codex-plugin" / "plugin.json"
CHANNEL_CC = REPO / "plugins" / "hive-channel" / ".claude-plugin" / "plugin.json"
ALL_MANIFESTS = [CC_MARKETPLACE, CODEX_MARKETPLACE, HIVE_CC, HIVE_CODEX, CHANNEL_CC]

CANONICAL_SKILL = REPO / "plugins" / "hive" / "skills" / "hive" / "SKILL.md"
SKILL_SYMLINKS = [
    REPO / "skills" / "hive" / "SKILL.md",
    REPO / "src" / "hive" / "core_assets" / "skills" / "hive" / "SKILL.md",
]

SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def _load(path: Path) -> dict:
    return json.loads(path.read_text())


def test_all_manifests_parse():
    for path in ALL_MANIFESTS:
        assert isinstance(_load(path), dict), path


def test_marketplace_name_is_not_legacy():
    for path in (CC_MARKETPLACE, CODEX_MARKETPLACE):
        name = _load(path)["name"]
        assert name == "hive-plugins", path
        assert name != claude_channel.MARKETPLACE_NAME


def test_cc_marketplace_lists_both_plugins_with_real_sources():
    entries = {p["name"]: p for p in _load(CC_MARKETPLACE)["plugins"]}
    assert set(entries) == {"hive", "hive-channel"}
    for entry in entries.values():
        source = entry["source"]
        assert source.startswith("./"), entry
        assert (REPO / source).is_dir(), entry


def test_codex_marketplace_lists_only_hive():
    data = _load(CODEX_MARKETPLACE)
    entries = data["plugins"]
    assert [p["name"] for p in entries] == ["hive"]
    entry = entries[0]
    assert entry["source"] == {"source": "local", "path": "./plugins/hive"}
    assert (REPO / entry["source"]["path"]).is_dir()
    assert "installation" in entry["policy"]
    assert "authentication" in entry["policy"]
    assert "category" in entry
    raw = CODEX_MARKETPLACE.read_text()
    assert "hive-channel" not in raw
    assert "channels" not in raw


def test_channel_manifest_is_portable():
    data = _load(CHANNEL_CC)
    server = data["mcpServers"]["hive-channel"]
    assert server["command"] == "hive"
    assert server["args"] == ["claude", "channel-server"]
    assert data["channels"] == [{"server": "hive-channel"}]


def test_no_machine_paths_in_any_manifest():
    forbidden = ["/Users/", "/private/", "PYTHONPATH", "HIVE_HOME", str(REPO)]
    for path in ALL_MANIFESTS:
        raw = path.read_text()
        for needle in forbidden:
            assert needle not in raw, f"{path}: {needle}"


def test_plugin_versions_are_semver_and_dual_manifests_agree():
    hive_cc = _load(HIVE_CC)["version"]
    hive_codex = _load(HIVE_CODEX)["version"]
    channel = _load(CHANNEL_CC)["version"]
    assert hive_cc == hive_codex
    for version in (hive_cc, channel):
        assert SEMVER.match(version), version


def test_canonical_skill_is_regular_and_symlinks_resolve_to_it():
    assert CANONICAL_SKILL.is_file() and not CANONICAL_SKILL.is_symlink()
    for link in SKILL_SYMLINKS:
        assert link.is_symlink(), link
        assert link.resolve() == CANONICAL_SKILL.resolve(), link


def test_skill_sync_canonical_bytes_match_canonical_file():
    assert skill_sync._canonical_hive_skill_bytes() == CANONICAL_SKILL.read_bytes()
