from pathlib import Path

from hive import skill_sync


def _configure_skill_homes(monkeypatch, tmp_path: Path) -> None:
    hive_home = tmp_path / ".hive"
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("HIVE_HOME", str(hive_home))
    monkeypatch.setenv("FACTORY_HOME", str(tmp_path / ".factory"))
    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path / ".claude"))
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / ".codex"))
    monkeypatch.setattr("hive.team.HIVE_HOME", hive_home)


def test_packaged_hive_skill_matches_repo_skill():
    repo_skill = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_bytes()

    assert skill_sync._canonical_hive_skill_bytes() == repo_skill


def test_diagnose_hive_skill_current_for_target_cli(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    skill_path = tmp_path / ".codex" / "skills" / "hive" / "SKILL.md"
    skill_path.parent.mkdir(parents=True, exist_ok=True)
    skill_path.write_bytes(skill_sync._canonical_hive_skill_bytes())

    payload = skill_sync.diagnose_hive_skill("codex")

    assert payload["state"] == "current"
    assert payload["cli"] == "codex"
    assert payload["installedPath"] == str(skill_path)
    assert payload["actualHash"] == payload["expectedHash"]


def test_diagnose_hive_skill_reports_missing_install(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)

    payload = skill_sync.diagnose_hive_skill("claude")

    assert payload["state"] == "missing"
    assert payload["recommendedAction"] == "refresh"
    assert payload["installedExists"] is False
    assert "--all" in payload["refreshCommand"]


def test_warn_hive_skill_drift_is_rate_limited(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    skill_path = tmp_path / ".codex" / "skills" / "hive" / "SKILL.md"
    skill_path.parent.mkdir(parents=True, exist_ok=True)
    skill_path.write_text("stale\n")
    messages: list[str] = []

    first = skill_sync.maybe_warn_hive_skill_drift("codex", emit=messages.append, now=100.0)
    second = skill_sync.maybe_warn_hive_skill_drift("codex", emit=messages.append, now=200.0)
    third = skill_sync.maybe_warn_hive_skill_drift(
        "codex",
        emit=messages.append,
        now=100.0 + 24 * 60 * 60 + 1,
    )

    assert first["state"] == "stale"
    assert second["state"] == "stale"
    assert third["state"] == "stale"
    assert len(messages) == 2
    state_file = tmp_path / ".hive" / "state" / "skill-sync" / "codex.json"
    assert state_file.is_file()


def test_check_version_upgrade_records_initial_version_without_warning(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    monkeypatch.setattr("hive.skill_sync.metadata.version", lambda _name: "0.4.1")
    messages: list[str] = []

    payload = skill_sync.check_version_upgrade(emit=messages.append)

    assert payload["state"] == "initialized"
    assert messages == []
    assert (tmp_path / ".hive" / "state" / "last_seen_version").read_text().strip() == "0.4.1"


def test_check_version_upgrade_warns_once_when_version_changes(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    state_path = tmp_path / ".hive" / "state" / "last_seen_version"
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text("0.4.0\n")
    monkeypatch.setattr("hive.skill_sync.metadata.version", lambda _name: "0.4.1")
    messages: list[str] = []

    payload = skill_sync.check_version_upgrade(emit=messages.append)

    assert payload["state"] == "upgraded"
    assert payload["previousVersion"] == "0.4.0"
    assert payload["currentVersion"] == "0.4.1"
    assert len(messages) == 1
    assert "hive upgraded from 0.4.0 to 0.4.1" in messages[0]
    assert state_path.read_text().strip() == "0.4.1"


def test_check_version_upgrade_reports_downgrade_when_cli_is_older(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    state_path = tmp_path / ".hive" / "state" / "last_seen_version"
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text("0.4.5\n")
    monkeypatch.setattr("hive.skill_sync.metadata.version", lambda _name: "0.4.3")
    messages: list[str] = []

    payload = skill_sync.check_version_upgrade(emit=messages.append)

    assert payload["state"] == "downgraded"
    assert "hive downgraded from 0.4.5 to 0.4.3" in messages[0]
    assert state_path.read_text().strip() == "0.4.3"


def test_check_version_upgrade_falls_back_to_changed_when_unparseable(monkeypatch, tmp_path: Path):
    _configure_skill_homes(monkeypatch, tmp_path)
    state_path = tmp_path / ".hive" / "state" / "last_seen_version"
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text("0.4.5-dev\n")
    monkeypatch.setattr("hive.skill_sync.metadata.version", lambda _name: "0.5.0")
    messages: list[str] = []

    payload = skill_sync.check_version_upgrade(emit=messages.append)

    assert payload["state"] == "changed"
    assert "hive changed from 0.4.5-dev to 0.5.0" in messages[0]
