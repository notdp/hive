"""Agent.send dispatch for Claude panes: strictly channel-only, no keystrokes."""
from __future__ import annotations

import os
import shutil
import tempfile
from pathlib import Path

import pytest

from hive import agent as agent_mod
from hive.adapters import claude_channel
from hive.agent import DeliveryError, Agent

pytestmark = pytest.mark.cli


@pytest.fixture
def _hive_home(monkeypatch):
    # Short base required: AF_UNIX sun_path caps at ~104 bytes on macOS, so
    # per-pane sockets cannot live under pytest's long tmp_path.
    base = "/tmp" if os.path.isdir("/tmp") else tempfile.gettempdir()
    home = Path(tempfile.mkdtemp(prefix="hh", dir=base))
    monkeypatch.setenv("HIVE_HOME", str(home))
    yield home
    shutil.rmtree(home, ignore_errors=True)


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


def test_claude_send_raises_when_channel_reports_failure(monkeypatch):
    keystrokes = _patch(monkeypatch, "claude")
    monkeypatch.setattr(claude_channel, "send_to_pane", lambda pane, text: None)

    # strictly channel-only: a failed channel is an explicit submit failure
    # (the sidecar projects the raise to injectStatus=failed), never keystrokes.
    with pytest.raises(DeliveryError):
        _agent("claude").send("<HIVE>hi</HIVE>")

    assert keystrokes == []


def test_claude_send_raises_without_ready_marker(monkeypatch, _hive_home):
    # real send_to_pane path: channel never registered for this pane
    keystrokes = _patch(monkeypatch, "claude")

    with pytest.raises(DeliveryError):
        _agent("claude").send("<HIVE>hi</HIVE>")

    assert keystrokes == []


def test_claude_send_raises_with_marker_but_dead_socket(monkeypatch, _hive_home):
    # real send_to_pane path: marker present but nothing is listening
    keystrokes = _patch(monkeypatch, "claude")
    marker = claude_channel.ready_marker_path("%1")
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("1")
    sock = claude_channel.channel_socket_path("%1")
    sock.write_text("")  # a plain file, not a live socket

    with pytest.raises(DeliveryError):
        _agent("claude").send("<HIVE>hi</HIVE>")

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
