"""Agent.send dispatch for Claude panes: channel first, send-keys fallback."""
from __future__ import annotations

import pytest

from hive import agent as agent_mod
from hive.adapters import claude_channel
from hive.agent import Agent

pytestmark = pytest.mark.cli


def _agent(cli: str = "claude") -> Agent:
    return Agent(name="w", team_name="t", pane_id="%1", cli=cli)


def _patch(monkeypatch, profile: str):
    monkeypatch.setattr(agent_mod, "_resolve_profile_name", lambda pane, cli: profile)
    keystrokes: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        agent_mod, "_submit_interactive_text",
        lambda pane, text, cli: keystrokes.append((pane, text, cli)),
    )
    return keystrokes


def test_claude_send_uses_channel_and_skips_keystrokes(monkeypatch):
    keystrokes = _patch(monkeypatch, "claude")
    calls: list[tuple[str, str]] = []
    monkeypatch.setattr(claude_channel, "send_to_pane",
                        lambda pane, text: calls.append((pane, text)) or True)

    _agent("claude").send("<HIVE>hi</HIVE>")

    assert calls == [("%1", "<HIVE>hi</HIVE>")]
    assert keystrokes == []  # channel succeeded -> no send-keys


def test_claude_send_does_not_fall_back_to_keystrokes(monkeypatch):
    keystrokes = _patch(monkeypatch, "claude")
    monkeypatch.setattr(claude_channel, "send_to_pane", lambda pane, text: False)

    _agent("claude").send("<HIVE>hi</HIVE>")

    # channel-only: even when the channel reports failure, claude never types
    # into the composer (a failed delivery surfaces via msgId-render tracking).
    assert keystrokes == []


def test_codex_send_path_unchanged(monkeypatch):
    keystrokes = _patch(monkeypatch, "codex")
    channel_calls: list = []
    monkeypatch.setattr(claude_channel, "send_to_pane",
                        lambda pane, text: channel_calls.append(1) or True)
    from hive.adapters import codex_app_server
    monkeypatch.setattr(codex_app_server, "send_to_pane", lambda pane, text: True)

    _agent("codex").send("<HIVE>hi</HIVE>")

    assert channel_calls == []  # codex never touches the claude channel
    assert keystrokes == []  # codex app-server handled it
