"""Routing contract of the /cvim popup sendback.

A pane with a native engine address never takes keystrokes: the sendback is
one interrupt plus one send over that CLI's own transport, and the exit code
is the routing decision the bash caller reads (0 delivered, 10 fall back to
the tmux keystroke chain, 1 refuse and keep the payload).
"""
from __future__ import annotations

import importlib.machinery
import importlib.util
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "src" / "hive" / "core_assets" / "cvim" / "bin" / "cvim-sendback"


def _load_helper():
    spec = importlib.util.spec_from_loader(
        "cvim_sendback",
        importlib.machinery.SourceFileLoader("cvim_sendback", str(HELPER)),
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sendback = _load_helper()


class _Calls(list):
    def names(self) -> list[str]:
        return [name for name, _ in self]


@pytest.fixture
def calls():
    return _Calls()


def _patch(monkeypatch, module_path: str, calls: _Calls, **answers):
    for name, answer in answers.items():
        def make(name=name, answer=answer):
            def fake(*args, **kwargs):
                calls.append((name, args))
                return answer(*args) if callable(answer) else answer
            return fake
        monkeypatch.setattr(f"{module_path}.{name}", make())


def _codex(monkeypatch, calls, **answers):
    _patch(monkeypatch, "hive.adapters.codex_app_server", calls, **answers)


def _grok(monkeypatch, calls, **answers):
    _patch(monkeypatch, "hive.adapters.grok_leader", calls, **answers)


# --- codex ---------------------------------------------------------------

def test_codex_thread_takes_the_interrupt_then_the_turn(monkeypatch, calls):
    _codex(
        monkeypatch, calls,
        thread_id_for_pane="th-1",
        interrupt_pane="turnInterruptAccepted",
        send_to_pane="turnStartAccepted",
    )

    code, fields = sendback.sendback("%3", "codex", "<comment on=\"x\">\n+edit\n</comment>", True)

    assert code == 0
    assert fields == {
        "route": "codexThread",
        "interrupt": "turnInterruptAccepted",
        "send": "turnStartAccepted",
    }
    assert calls.names() == ["thread_id_for_pane", "interrupt_pane", "send_to_pane"]
    assert calls[-1][1] == ("%3", "<comment on=\"x\">\n+edit\n</comment>")


def test_codex_idle_thread_reports_no_running_turn_and_still_sends(monkeypatch, calls):
    _codex(
        monkeypatch, calls,
        thread_id_for_pane="th-1",
        interrupt_pane="noRunningTurn",
        send_to_pane="turnStartAccepted",
    )

    code, fields = sendback.sendback("%3", "codex", "edited", True)

    assert code == 0
    assert fields["interrupt"] == "noRunningTurn"
    assert fields["send"] == "turnStartAccepted"


def test_codex_interrupt_knob_off_only_sends(monkeypatch, calls):
    _codex(
        monkeypatch, calls,
        thread_id_for_pane="th-1",
        interrupt_pane="turnInterruptAccepted",
        send_to_pane="turnStartAccepted",
    )

    code, fields = sendback.sendback("%3", "codex", "edited", False)

    assert code == 0
    assert "interrupt" not in fields
    assert calls.names() == ["thread_id_for_pane", "send_to_pane"]


def test_codex_unedited_popup_only_interrupts(monkeypatch, calls):
    _codex(
        monkeypatch, calls,
        thread_id_for_pane="th-1",
        interrupt_pane="turnInterruptAccepted",
        send_to_pane="turnStartAccepted",
    )

    code, fields = sendback.sendback("%3", "codex", None, True)

    assert code == 0
    assert calls.names() == ["thread_id_for_pane", "interrupt_pane"]
    assert "send" not in fields


def test_codex_without_a_thread_record_falls_back_to_keystrokes(monkeypatch, calls):
    _codex(
        monkeypatch, calls,
        thread_id_for_pane=None,
        interrupt_pane="turnInterruptAccepted",
        send_to_pane="turnStartAccepted",
    )

    code, fields = sendback.sendback("%3", "codex", "edited", True)

    assert code == sendback.NO_NATIVE_ADDRESS
    assert fields == {"route": "tmuxKeys", "why": "no_recorded_thread"}
    assert calls.names() == ["thread_id_for_pane"]


def test_codex_transport_refusal_never_falls_back_to_keystrokes(monkeypatch, calls):
    # The thread record is the address; a daemon that refused it is a failed
    # delivery, not an unmanaged pane — the caller keeps the payload.
    _codex(
        monkeypatch, calls,
        thread_id_for_pane="th-1",
        interrupt_pane=None,
        send_to_pane=None,
    )

    code, fields = sendback.sendback("%3", "codex", "edited", True)

    assert code == sendback.REFUSED
    assert fields == {"route": "codexThread", "interrupt": "failed", "send": "failed"}


@pytest.mark.parametrize("profile", ["codex", "grok"])
def test_a_slash_command_goes_to_the_composer_not_the_rpc(monkeypatch, calls, profile):
    # turn/start and session/prompt carry prompts; a slash command sent as
    # one only feeds the model the literal "/compact".
    _codex(monkeypatch, calls, thread_id_for_pane="th-1", send_to_pane="turnStartAccepted")
    _grok(monkeypatch, calls, session_id_for_pane="sess-1", send_to_pane="sessionPromptQueued")

    code, fields = sendback.sendback("%3", profile, "/compact", True)

    assert code == sendback.NO_NATIVE_ADDRESS
    assert fields == {"route": "tmuxKeys", "why": "slash_command"}
    assert calls.names() == []


def test_a_multiline_payload_that_starts_with_a_slash_is_a_prompt(monkeypatch, calls):
    _codex(monkeypatch, calls, thread_id_for_pane="th-1", send_to_pane="turnStartAccepted")

    code, fields = sendback.sendback("%3", "codex", "/tmp/x is the path\nfix it", False)

    assert code == 0
    assert fields["send"] == "turnStartAccepted"


# --- grok ----------------------------------------------------------------

def test_grok_session_takes_the_cancel_then_the_prompt(monkeypatch, calls):
    _grok(
        monkeypatch, calls,
        session_id_for_pane="sess-1",
        interrupt_pane="sessionCancelSent",
        send_to_pane="sessionPromptQueued",
    )

    code, fields = sendback.sendback("%7", "grok", "edited", True)

    assert code == 0
    assert fields == {
        "route": "grokSession",
        "interrupt": "sessionCancelSent",
        "send": "sessionPromptQueued",
    }
    assert calls.names() == ["session_id_for_pane", "interrupt_pane", "send_to_pane"]


def test_grok_without_a_session_record_falls_back_to_keystrokes(monkeypatch, calls):
    _grok(
        monkeypatch, calls,
        session_id_for_pane=None,
        interrupt_pane="sessionCancelSent",
        send_to_pane="sessionPromptQueued",
    )

    code, fields = sendback.sendback("%7", "grok", "edited", True)

    assert code == sendback.NO_NATIVE_ADDRESS
    assert fields["route"] == "tmuxKeys"
    assert calls.names() == ["session_id_for_pane"]


# --- claude --------------------------------------------------------------

class _KeyResult:
    def __init__(self, ok: bool, confirmed: str = "", why: str = ""):
        self.ok, self.confirmed, self.why = ok, confirmed, why


def _claude(monkeypatch, calls, **answers):
    _patch(monkeypatch, "hive.adapters.claude_bg", calls, **{
        k: v for k, v in answers.items() if k in {"job_id_for_pane", "interrupt_job", "type_into_job"}
    })
    if "interactive_claude_pid" in answers:
        _patch(monkeypatch, "hive.adapters.claude_view", calls,
               interactive_claude_pid=answers["interactive_claude_pid"])


def test_claude_job_pipes_the_interrupt_and_the_text(monkeypatch, calls):
    _claude(
        monkeypatch, calls,
        job_id_for_pane="cafe1234",
        interrupt_job=_KeyResult(True, "status"),
        type_into_job=_KeyResult(True, "transcript"),
    )

    code, fields = sendback.sendback("%1", "claude", "edited", True)

    assert code == 0
    assert fields == {
        "route": "claudeJobPipe",
        "job": "cafe1234",
        "interrupt": "status",
        "send": "transcript",
    }
    assert calls.names() == ["job_id_for_pane", "interrupt_job", "type_into_job"]


def test_claude_slash_command_stays_on_the_pipe(monkeypatch, calls):
    # claude's pipe types into the engine's own composer, so a slash command
    # runs there as a command; only the RPC CLIs have to detour.
    _claude(
        monkeypatch, calls,
        job_id_for_pane="cafe1234",
        interrupt_job=_KeyResult(True, "written"),
        type_into_job=_KeyResult(True, "transcript"),
    )

    code, fields = sendback.sendback("%1", "claude", "/compact", True)

    assert code == 0
    assert fields["route"] == "claudeJobPipe"
    assert calls.names() == ["job_id_for_pane", "interrupt_job", "type_into_job"]


def test_claude_pane_without_a_job_but_with_a_tui_falls_back_to_keystrokes(monkeypatch, calls):
    _claude(monkeypatch, calls, job_id_for_pane=None, interactive_claude_pid=4321)

    code, fields = sendback.sendback("%1", "claude", "edited", True)

    assert code == sendback.NO_NATIVE_ADDRESS
    assert fields == {"route": "tmuxKeys", "why": "no_job_record"}


def test_claude_pane_with_neither_job_nor_tui_is_refused(monkeypatch, calls):
    _claude(monkeypatch, calls, job_id_for_pane=None, interactive_claude_pid=None)

    code, fields = sendback.sendback("%1", "claude", "edited", True)

    assert code == sendback.REFUSED
    assert fields["route"] == "none"


def test_claude_pipe_failure_never_falls_back_to_keystrokes(monkeypatch, calls):
    _claude(
        monkeypatch, calls,
        job_id_for_pane="cafe1234",
        interrupt_job=_KeyResult(True, "written"),
        type_into_job=_KeyResult(False, why="never echoed"),
    )

    code, fields = sendback.sendback("%1", "claude", "edited", True)

    assert code == sendback.REFUSED
    assert fields["send"] == "failed"
    assert fields["why"] == "never echoed"


# --- unmanaged CLIs ------------------------------------------------------

@pytest.mark.parametrize("profile", ["", "unknown", "aider"])
def test_a_pane_running_no_supported_cli_falls_back_to_keystrokes(profile):
    code, fields = sendback.sendback("%9", profile, "edited", True)

    assert code == sendback.NO_NATIVE_ADDRESS
    assert fields["route"] == "tmuxKeys"
