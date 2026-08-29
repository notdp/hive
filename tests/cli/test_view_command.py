"""hive view: read-only transcript viewer, external renderer preferred."""
from pathlib import Path

import pytest
from click.testing import CliRunner

from hive.cli import cli

pytestmark = pytest.mark.cli


def test_view_execs_tail_claude_on_the_transcript(monkeypatch, tmp_path):
    transcript = tmp_path / "sid-1.jsonl"
    transcript.write_text("{}\n")
    execs = []
    monkeypatch.setattr("hive.transcript_view.external_viewer", lambda: "/opt/bin/tail-claude")
    monkeypatch.setattr("hive.transcript_view.transcript_path", lambda sid: transcript)
    monkeypatch.setattr(
        "hive.cli.os.execv",
        lambda exe, argv: execs.append((exe, argv)) or (_ for _ in ()).throw(SystemExit(0)),
    )
    result = CliRunner().invoke(cli, ["view", "sid-1"])
    assert result.exit_code == 0
    assert execs == [("/opt/bin/tail-claude", ["/opt/bin/tail-claude", str(transcript)])]


def test_view_falls_back_to_builtin_renderer_without_viewer(monkeypatch):
    followed = []
    monkeypatch.setattr("hive.transcript_view.external_viewer", lambda: None)
    monkeypatch.setattr("hive.transcript_view.follow", lambda sid: followed.append(sid) or 0)
    result = CliRunner().invoke(cli, ["view", "sid-2"])
    assert result.exit_code == 0
    assert followed == ["sid-2"]


def test_view_falls_back_when_no_transcript_exists(monkeypatch):
    followed = []
    monkeypatch.setattr("hive.transcript_view.external_viewer", lambda: "/opt/bin/tail-claude")
    monkeypatch.setattr("hive.transcript_view.transcript_path", lambda sid: None)
    monkeypatch.setattr("hive.transcript_view.follow", lambda sid: followed.append(sid) or 1)
    result = CliRunner().invoke(cli, ["view", "ghost"])
    assert result.exit_code == 1
    assert followed == ["ghost"]
