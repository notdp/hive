"""Launcher-minted job/thread names carry the member identity.

The claude agents panel and ledger label a bg job by its name; the view
probe's title branch matches `<team>.<member>`. A launcher mint on a tagged
member pane must therefore name the job/thread after the member, and only
untagged (non-member) panes fall back to the pane-derived placeholder.
"""
import pytest

from hive import cli


pytestmark = pytest.mark.unit


def _tags(monkeypatch, mapping):
    def fake_get(target, key):
        return mapping.get((target, key))

    monkeypatch.setattr(cli.tmux, "get_window_option", fake_get)


def test_a_member_pane_mints_the_member_name_for_claude(monkeypatch):
    _tags(monkeypatch, {("%179", "hive-team"): "honey", ("%179", "hive-agent"): "worker"})
    assert cli._claude_pane_job_name("%179") == "honey.worker"


def test_a_member_pane_mints_the_member_name_for_codex(monkeypatch):
    _tags(monkeypatch, {("%9", "hive-team"): "comb", ("%9", "hive-agent"): "validator"})
    assert cli._codex_pane_thread_name("%9") == "comb.validator"


def test_an_untagged_pane_falls_back_to_the_pane_placeholder(monkeypatch):
    _tags(monkeypatch, {})
    assert cli._claude_pane_job_name("%42") == "hive-42"
    assert cli._codex_pane_thread_name("%42") == "hive-42"


def test_a_half_tagged_pane_is_not_a_member(monkeypatch):
    _tags(monkeypatch, {("%7", "hive-team"): "honey"})
    assert cli._claude_pane_job_name("%7") == "hive-7"
