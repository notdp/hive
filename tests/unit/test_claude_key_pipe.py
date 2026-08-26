"""A claude member's keyboard is its job, typed at over `claude attach`.

The pipe carries text and control keys into the engine pty with no tmux and
no viewer in the path. What is asserted here is the contract the real machine
proved: the composer is cleared in its own chunk before typing, readiness is
the engine echoing the text back (not a sleep), a lost keystroke is re-typed
idempotently, and the submit only counts once the transcript shows the turn.
"""

import json

import pytest

from hive import agent
from hive.adapters import claude_bg

pytestmark = pytest.mark.unit


class FakePipe:
    """Stand-in for the `claude attach` client: records what was written."""

    def __init__(self, *, broken_after: int | None = None):
        self.writes: list[str] = []
        self.pid = 4242
        self.closed = False
        self.killed = False
        self.stdin = self
        self._broken_after = broken_after

    # stdin surface
    def write(self, payload: str) -> None:
        if self._broken_after is not None and len(self.writes) >= self._broken_after:
            raise BrokenPipeError("client gone")
        self.writes.append(payload)

    def flush(self) -> None:
        pass

    def close(self) -> None:
        self.closed = True

    # process surface
    def poll(self):
        return None

    def wait(self, timeout=None):
        return 0

    def kill(self):
        self.killed = True


def _engine(job_id: str = "cafe1234", status: str = "idle") -> claude_bg.EngineSession:
    return claude_bg.EngineSession(
        pid=999,
        job_id=job_id,
        session_id="sid-1",
        socket_path="/tmp/sock",
        cwd="/repo",
        status=status,
        waiting_for="",
        status_updated_at=0.0,
    )


def _wire(monkeypatch, pipe, *, screens, transcript=None, engine=None, baseline="> ", draft=False):
    """Attach *pipe*, feed `claude logs` from *screens*, transcript from a file.

    *baseline* is what the screen shows before anything is typed — the pipe
    reads it first and only counts an echo that was not already there.
    *draft* is what the dim-aware composer parser reports before the C-u.
    """
    monkeypatch.setattr(claude_bg, "_attach_pipe", lambda job, **kw: pipe)
    monkeypatch.setattr(claude_bg, "_wait_client_ready", lambda proc: True)
    monkeypatch.setattr(claude_bg, "_wait_engine_behind", lambda job, proc: engine or _engine())
    monkeypatch.setattr(claude_bg, "_transcript_cursor", lambda eng: (transcript, 0))
    monkeypatch.setattr(claude_bg, "_composer_has_draft", lambda job, **kw: draft)
    monkeypatch.setattr(claude_bg.time, "sleep", lambda _s: None)
    feed = [baseline, *screens]
    monkeypatch.setattr(
        claude_bg, "job_screen", lambda job, **kw: feed.pop(0) if feed else screens[-1]
    )


def _transcript(tmp_path, records):
    path = tmp_path / "session.jsonl"
    path.write_text("".join(json.dumps(r) + "\n" for r in records))
    return path


def _user(text):
    return {"type": "user", "message": {"role": "user", "content": text}}


# --- argv shape ------------------------------------------------------------


def test_attach_and_logs_put_the_subcommand_first(monkeypatch):
    """Hidden subcommands are only recognized at argv[1]; a leading flag
    silently downgrades `attach` into a prompt."""
    seen: list[list[str]] = []

    class Result:
        returncode = 0
        stdout = "screen"

    monkeypatch.setattr(
        claude_bg.subprocess, "run", lambda argv, **kw: seen.append(argv) or Result()
    )
    monkeypatch.setattr(
        claude_bg.subprocess, "Popen", lambda argv, **kw: seen.append(argv) or FakePipe()
    )

    claude_bg.job_screen("cafe1234")
    claude_bg._attach_pipe("cafe1234", claude_bin="claude")

    assert seen == [["claude", "logs", "cafe1234"], ["claude", "attach", "cafe1234"]]


def test_pipe_env_is_washed_of_claude_vars(monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_CHILD_SESSION", "1")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "secret")
    env = claude_bg.bg_env()
    assert "CLAUDE_CODE_CHILD_SESSION" not in env and "ANTHROPIC_API_KEY" not in env


# --- typing ----------------------------------------------------------------


def test_typing_clears_the_composer_in_its_own_chunk_then_submits(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("hello there")])
    _wire(monkeypatch, pipe, screens=["> hello there"], transcript=path)

    result = claude_bg.type_into_job("cafe1234", "hello there")

    assert result.ok and result.confirmed == "transcript"
    # C-u alone, then the text, then Enter — a control byte must never ride in
    # the text's chunk (it gets inserted literally when it does).
    assert pipe.writes == ["\x15", "hello there", "\r"]
    assert pipe.closed


def test_a_lost_keystroke_is_retyped_and_the_retype_cannot_double(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("ping")])
    # First screens have no echo: the client was not forwarding yet.
    _wire(monkeypatch, pipe, screens=["> ", "> ", "> ping"], transcript=path)
    monkeypatch.setattr(claude_bg, "_TYPE_RETRY_AFTER", 0.0)

    assert claude_bg.type_into_job("cafe1234", "ping").ok
    # Every retype re-clears first, so the composer holds one copy, not two.
    assert pipe.writes.count("ping") == pipe.writes.count("\x15")
    assert pipe.writes[-1] == "\r"


def test_no_echo_within_the_budget_refuses_instead_of_submitting(monkeypatch, tmp_path):
    pipe = FakePipe()
    _wire(monkeypatch, pipe, screens=["> something else"], transcript=tmp_path / "none.jsonl")
    monkeypatch.setattr(claude_bg, "_TYPE_READY_TIMEOUT", 0.0)

    result = claude_bg.type_into_job("cafe1234", "ping")

    assert not result.ok and "\r" not in pipe.writes


def test_the_echo_survives_the_composer_wrapping_the_text(monkeypatch, tmp_path):
    """`claude logs` is a raw pty replay: the layout is cursor moves and box
    drawing, so the echo is matched with both squashed out."""
    pipe = FakePipe()
    text = "a long sendback that the composer wraps over two lines"
    path = _transcript(tmp_path, [_user(text)])
    wrapped = "╭─────────╮\n│ a long sendback that the │\n│ composer wraps over two lines │\n╰──╯"
    _wire(monkeypatch, pipe, screens=[wrapped], transcript=path)

    assert claude_bg.type_into_job("cafe1234", text).ok


def test_text_already_on_the_screen_is_not_taken_for_the_echo(monkeypatch, tmp_path):
    """The screen is the tail of the whole pty replay, so the same sendback
    delivered twice (or a payload quoting what is displayed) is already there
    before a byte is typed. Only a *new* copy proves the client forwarded."""
    pipe = FakePipe()
    stale = "> ping\n(the previous delivery, still in the scrollback)"
    _wire(monkeypatch, pipe, screens=[stale], transcript=tmp_path / "none.jsonl", baseline=stale)
    monkeypatch.setattr(claude_bg, "_TYPE_READY_TIMEOUT", 0.01)

    result = claude_bg.type_into_job("cafe1234", "ping")

    assert not result.ok and "ping" in pipe.writes and "\r" not in pipe.writes


def test_a_second_copy_of_the_same_text_is_the_echo(monkeypatch, tmp_path):
    pipe = FakePipe()
    stale = "> ping\n(the previous delivery, still in the scrollback)"
    path = _transcript(tmp_path, [_user("ping")])
    _wire(monkeypatch, pipe, screens=[stale + "\n> ping"], transcript=path, baseline=stale)

    assert claude_bg.type_into_job("cafe1234", "ping").ok


def test_a_long_sendback_echoes_by_its_tail(monkeypatch, tmp_path):
    """The composer scrolls to the cursor, so a long paste shows its end and
    the head never reaches the screen."""
    pipe = FakePipe()
    text = "head of the sendback\n" + "filler line\n" * 40 + "the very last line of it"
    path = _transcript(tmp_path, [_user(text)])
    viewport = "│ filler line │\n" * 5 + "│ the very last line of it │"
    _wire(monkeypatch, pipe, screens=[viewport], transcript=path)

    assert claude_bg.type_into_job("cafe1234", text).ok


def test_a_pasted_text_placeholder_counts_as_the_echo(monkeypatch, tmp_path):
    """A long paste is folded into `[Pasted text #N]`: none of the text is on
    screen, and the placeholder is the only thing the client can echo."""
    pipe = FakePipe()
    text = "a sendback long enough for the TUI to fold it away\n" * 20
    path = _transcript(tmp_path, [_user(text)])
    earlier = "> [Pasted text #1 +3 lines]"  # an older paste, still in the replay
    _wire(
        monkeypatch,
        pipe,
        screens=[earlier + "\n> [Pasted text #2 +20 lines]"],
        transcript=path,
        baseline=earlier,
    )

    assert claude_bg.type_into_job("cafe1234", text).ok


def test_a_removed_job_fails_as_soon_as_the_client_gives_up(monkeypatch, tmp_path):
    """`attach <gone>` exits at once; waiting out the wake budget for an
    engine that will never register just delays the error."""
    pipe = FakePipe()
    pipe.poll = lambda: 1
    monkeypatch.setattr(claude_bg, "_attach_pipe", lambda job, **kw: pipe)
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: None)
    monkeypatch.setattr(claude_bg.time, "sleep", lambda _s: pytest.fail("must not poll on"))

    result = claude_bg.type_into_job("deadbeef", "ping")

    assert not result.ok and "no engine" in result.why


def test_a_broken_pipe_is_a_failure_not_a_crash(monkeypatch, tmp_path):
    pipe = FakePipe(broken_after=0)
    _wire(monkeypatch, pipe, screens=["> "], transcript=tmp_path / "none.jsonl")

    result = claude_bg.type_into_job("cafe1234", "ping")

    assert not result.ok and "stdin" in result.why


# --- submit confirmation ---------------------------------------------------


def test_a_turn_that_swallowed_a_leftover_draft_is_not_confirmed(monkeypatch, tmp_path):
    """The transcript turn must equal what was typed. A composer that still
    held a draft produces a longer turn — the one thing a substring match
    would wave through."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("DRAFTJUNK/compact")])
    _wire(monkeypatch, pipe, screens=["> DRAFTJUNK/compact"], transcript=path)

    result = claude_bg.type_into_job("cafe1234", "/compact")

    assert not result.ok and "leftover draft" in result.why


def test_a_slash_command_is_confirmed_by_its_command_record(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("<command-name>/compact</command-name>")])
    _wire(monkeypatch, pipe, screens=["> /compact"], transcript=path)

    result = claude_bg.type_into_job("cafe1234", "/compact")

    assert result.ok and result.confirmed == "transcript"


def test_a_ui_only_slash_command_degrades_to_written(monkeypatch, tmp_path):
    """`/cost` and friends draw a panel and write nothing — silence there is
    not evidence the keystrokes were lost."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [])
    _wire(monkeypatch, pipe, screens=["> /cost"], transcript=path)
    monkeypatch.setattr(claude_bg, "_SLASH_CONFIRM_TIMEOUT", 0.0)

    result = claude_bg.type_into_job("cafe1234", "/cost")

    assert result.ok and result.confirmed == "written"


def test_plain_text_without_a_turn_is_a_failure(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [])
    _wire(monkeypatch, pipe, screens=["> ping"], transcript=path)
    monkeypatch.setattr(claude_bg, "_SUBMIT_CONFIRM_TIMEOUT", 0.0)

    assert not claude_bg.type_into_job("cafe1234", "ping").ok


# --- interrupt -------------------------------------------------------------


def test_interrupt_writes_one_escape_and_confirms_on_the_marker(monkeypatch, tmp_path):
    """Escape is never repeated: a second one lands on claude's own
    'edit previous message' chord."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [{"type": "system", "content": "[Request interrupted by user]"}])
    _wire(monkeypatch, pipe, screens=[""], transcript=path, engine=_engine(status="busy"))

    result = claude_bg.interrupt_job("cafe1234")

    assert result.ok and result.confirmed == "transcript"
    assert pipe.writes == ["\x1b"]


def test_interrupt_of_an_idle_engine_returns_at_once(monkeypatch, tmp_path):
    """Nothing is running, so nothing can confirm: sitting out the window
    could only relabel a success — and cvim sends this before every sendback."""
    pipe = FakePipe()
    _wire(monkeypatch, pipe, screens=[""], transcript=_transcript(tmp_path, []))
    monkeypatch.setattr(
        claude_bg,
        "engine_session_for_job",
        lambda job: pytest.fail("an idle engine must not be polled"),
    )

    result = claude_bg.interrupt_job("cafe1234")

    assert result.ok and result.confirmed == "written"


def test_interrupt_of_a_busy_engine_that_stays_busy_fails(monkeypatch, tmp_path):
    pipe = FakePipe()
    busy = _engine(status="busy")
    _wire(monkeypatch, pipe, screens=[""], transcript=_transcript(tmp_path, []), engine=busy)
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: busy)
    monkeypatch.setattr(claude_bg, "_INTERRUPT_CONFIRM_TIMEOUT", 0.0)

    assert not claude_bg.interrupt_job("cafe1234").ok


def test_interrupt_confirms_when_the_engine_leaves_busy(monkeypatch, tmp_path):
    pipe = FakePipe()
    busy = _engine(status="busy")
    _wire(monkeypatch, pipe, screens=[""], transcript=_transcript(tmp_path, []), engine=busy)
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: _engine())

    result = claude_bg.interrupt_job("cafe1234")

    assert result.ok and result.confirmed == "status"


# --- a wedged client may not outlive the call ------------------------------


def test_a_client_that_will_not_exit_is_killed(monkeypatch, tmp_path):
    pipe = FakePipe()

    def hang(timeout=None):
        raise claude_bg.subprocess.TimeoutExpired("claude attach", timeout or 0)

    pipe.wait = hang
    _wire(monkeypatch, pipe, screens=["> ping"], transcript=_transcript(tmp_path, [_user("ping")]))

    assert claude_bg.type_into_job("cafe1234", "ping").ok
    assert pipe.killed


# --- the hive paths that ride the pipe -------------------------------------


def _member_pane(monkeypatch, job_id):
    monkeypatch.setattr("hive.agent._resolve_profile_name", lambda pane, cli: "claude")
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _pane: job_id)


def _forbid_tmux(monkeypatch):
    def boom(*a, **kw):
        raise AssertionError("a claude member's keyboard must not touch tmux")

    for name in ("send_keys", "send_key", "is_pane_in_mode", "load_buffer", "paste_buffer"):
        monkeypatch.setattr(f"hive.agent.tmux.{name}", boom)


def test_submit_on_a_member_pane_pipes_into_the_job(monkeypatch):
    _member_pane(monkeypatch, "cafe1234")
    _forbid_tmux(monkeypatch)
    typed = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.type_into_job",
        lambda job, text, **kw: typed.append((job, text)) or claude_bg.KeyResult(True, "transcript"),
    )

    agent._submit_interactive_text("%1", "hello", "claude")

    assert typed == [("cafe1234", "hello")]


def test_submit_raises_when_the_job_did_not_take_the_text(monkeypatch):
    _member_pane(monkeypatch, "cafe1234")
    monkeypatch.setattr(
        "hive.adapters.claude_bg.type_into_job",
        lambda job, text, **kw: claude_bg.KeyResult(False, why="never echoed"),
    )

    with pytest.raises(RuntimeError, match="never echoed"):
        agent._submit_interactive_text("%1", "hello", "claude")


def test_a_non_member_claude_pane_still_goes_through_tmux(monkeypatch):
    """No job record: a plain interactive claude TUI, typed at like any other
    CLI — and refused when that TUI is not running."""
    _member_pane(monkeypatch, None)
    monkeypatch.setattr("hive.agent.tmux.is_pane_in_mode", lambda _pane: False)
    monkeypatch.setattr("hive.agent._save_and_clear_draft", lambda pane, profile: "")
    monkeypatch.setattr("hive.agent.time.sleep", lambda _s: None)
    sent = []
    monkeypatch.setattr("hive.agent.tmux.send_keys", lambda pane, text, **kw: sent.append(text))
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: sent.append(key))
    monkeypatch.setattr("hive.adapters.claude_view.viewer_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _pane: 456)

    agent._submit_interactive_text("%1", "hello", "claude")
    assert sent == ["hello", "Enter"]

    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _pane: None)
    with pytest.raises(RuntimeError, match="no interactive claude"):
        agent._submit_interactive_text("%1", "hello", "claude")


def test_a_pane_whose_claude_is_an_attach_viewer_is_refused(monkeypatch):
    """A lost job record must not fall back onto the pane: the claude process
    there is a viewer, and its composer belongs to whatever session it shows —
    another member's, or a stranger's."""
    _member_pane(monkeypatch, None)
    _forbid_tmux(monkeypatch)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _pane: 456)
    monkeypatch.setattr(
        "hive.adapters.claude_view.viewer_for_pane", lambda _pane: (456, "attach", "cafe1234")
    )

    with pytest.raises(RuntimeError, match="no interactive claude"):
        agent._submit_interactive_text("%1", "hello", "claude")


def test_member_interrupt_pipes_escape_into_the_job(monkeypatch):
    _member_pane(monkeypatch, "cafe1234")
    _forbid_tmux(monkeypatch)
    calls = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.interrupt_job",
        lambda job, **kw: calls.append(job) or claude_bg.KeyResult(True, "transcript"),
    )

    agent.Agent(name="red", team_name="probe", pane_id="%1", cli="claude").interrupt()

    assert calls == ["cafe1234"]


def test_interrupt_on_other_clis_still_sends_escape_to_the_pane(monkeypatch):
    keys = []
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: keys.append((pane, key)))

    agent.Agent(name="blue", team_name="probe", pane_id="%2", cli="codex").interrupt()

    assert keys == [("%2", "Escape")]


# --- draft save/restore ----------------------------------------------------


def test_a_killed_draft_is_pasted_back_after_the_submit(monkeypatch, tmp_path):
    """C-u parks the draft on claude's kill ring; a confirmed submit pastes
    it back (C-y) so the human's half-typed thought survives the command."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("hello there")])
    _wire(monkeypatch, pipe, screens=["> hello there"], transcript=path, draft=True)

    result = claude_bg.type_into_job("cafe1234", "hello there")

    assert result.ok
    assert pipe.writes == ["\x15", "hello there", "\r", "\x19"]


def test_an_empty_composer_never_gets_a_stale_ring_pasted(monkeypatch, tmp_path):
    """The kill ring survives a C-u that killed nothing; pasting it back
    would resurrect unrelated content (real-machine verified)."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("hello there")])
    _wire(monkeypatch, pipe, screens=["> hello there"], transcript=path, draft=False)

    assert claude_bg.type_into_job("cafe1234", "hello there").ok
    assert "\x19" not in pipe.writes


def test_a_retype_forfeits_the_restore(monkeypatch, tmp_path):
    """The second C-u overwrites the single-slot ring with our own failed
    text — pasting that back would fabricate a draft the human never typed."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("ping")])
    _wire(monkeypatch, pipe, screens=["> ", "> ", "> ping"], transcript=path, draft=True)
    monkeypatch.setattr(claude_bg, "_TYPE_RETRY_AFTER", 0.0)

    assert claude_bg.type_into_job("cafe1234", "ping").ok
    assert "\x19" not in pipe.writes


def test_a_slash_command_restores_the_draft_too(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [])
    _wire(monkeypatch, pipe, screens=["> /cost"], transcript=path, draft=True)
    monkeypatch.setattr(claude_bg, "_SLASH_CONFIRM_TIMEOUT", 0.0)

    result = claude_bg.type_into_job("cafe1234", "/cost")

    assert result.ok and result.confirmed == "written"
    assert pipe.writes[-1] == "\x19"


def test_a_failed_submit_does_not_touch_the_ring(monkeypatch, tmp_path):
    """On corruption the composer state is unknown — pasting on top of it
    could double the mess; the loud failure is the whole point."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [_user("DRAFT-hello there")])
    _wire(monkeypatch, pipe, screens=["> hello there"], transcript=path, draft=True)

    result = claude_bg.type_into_job("cafe1234", "hello there")

    assert not result.ok
    assert "\x19" not in pipe.writes
def test_the_draft_gate_reads_the_pane_only_when_it_shows_this_job(monkeypatch):
    """The logs replay is an incremental paint stream and cannot answer
    "what is in the composer"; the member's own pane render can — but only
    while it is actually showing this member."""
    from types import SimpleNamespace

    from hive import draft_guard
    from hive.adapters import claude_view

    monkeypatch.setattr(claude_bg, "pane_for_job", lambda job: "%7")
    monkeypatch.setattr(
        claude_view, "view_for_pane",
        lambda pane: SimpleNamespace(job_id="cafe1234", certainty="certain"),
    )
    seen = []
    monkeypatch.setattr(
        draft_guard, "suspected_draft", lambda pane, prof: seen.append((pane, prof)) or True
    )

    assert claude_bg._composer_has_draft("cafe1234") is True
    assert seen == [("%7", "claude")]


def test_the_draft_gate_is_closed_when_the_viewer_shows_someone_else(monkeypatch):
    from types import SimpleNamespace

    from hive import draft_guard
    from hive.adapters import claude_view

    monkeypatch.setattr(claude_bg, "pane_for_job", lambda job: "%7")
    monkeypatch.setattr(
        claude_view, "view_for_pane",
        lambda pane: SimpleNamespace(job_id="other999", certainty="certain"),
    )
    monkeypatch.setattr(
        draft_guard, "suspected_draft",
        lambda pane, prof: (_ for _ in ()).throw(AssertionError("must not capture")),
    )

    assert claude_bg._composer_has_draft("cafe1234") is False


def test_the_draft_gate_is_closed_without_a_pane(monkeypatch):
    monkeypatch.setattr(claude_bg, "pane_for_job", lambda job: None)
    assert claude_bg._composer_has_draft("cafe1234") is False


def test_a_probe_failure_closes_the_draft_gate(monkeypatch):
    from hive.adapters import claude_view

    monkeypatch.setattr(claude_bg, "pane_for_job", lambda job: "%7")
    monkeypatch.setattr(
        claude_view, "view_for_pane",
        lambda pane: (_ for _ in ()).throw(RuntimeError("tmux gone")),
    )
    assert claude_bg._composer_has_draft("cafe1234") is False


# --- job naming ------------------------------------------------------------


def _named_engine(name):
    return claude_bg.EngineSession(
        pid=1, job_id="cafe1234", session_id="s", socket_path="/tmp/s", cwd="/repo",
        status="idle", waiting_for="", status_updated_at=0.0, name=name,
    )


def test_a_wrongly_named_job_is_renamed_with_the_slash_command(monkeypatch):
    typed = []
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: _named_engine("hive-183"))
    monkeypatch.setattr(
        claude_bg, "type_into_job",
        lambda job, text, **kw: typed.append((job, text)) or claude_bg.KeyResult(True, "transcript"),
    )

    assert claude_bg.ensure_job_named("cafe1234", "honey.worker") is True
    assert typed == [("cafe1234", "/rename honey.worker")]


def test_a_correctly_named_job_types_nothing(monkeypatch):
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: _named_engine("honey.worker"))
    monkeypatch.setattr(
        claude_bg, "type_into_job",
        lambda job, text, **kw: (_ for _ in ()).throw(AssertionError("must not type")),
    )

    assert claude_bg.ensure_job_named("cafe1234", "honey.worker") is True


def test_naming_an_engineless_job_reports_failure(monkeypatch):
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: None)
    assert claude_bg.ensure_job_named("cafe1234", "honey.worker") is False


def test_the_registry_name_is_read_into_the_engine_session():
    engine = claude_bg._entry_to_engine({
        "kind": "bg", "pid": 1, "jobId": "cafe1234",
        "messagingSocketPath": __file__, "name": "honey.worker",
    })
    assert engine is not None and engine.name == "honey.worker"


def test_bg_env_keeps_color_forcing_for_the_renderer(monkeypatch):
    """Color is the engine's to keep — a cold-spawned engine renders its TUI
    with this env for its whole life. Safety against colored output lives at
    the parse sites (ANSI strip), never in the env."""
    from hive.adapters import claude_bg

    monkeypatch.setenv("FORCE_COLOR", "3")
    env = claude_bg.bg_env()
    assert env["FORCE_COLOR"] == "3"
    assert "NO_COLOR" not in env


def test_list_jobs_parses_colored_json(monkeypatch):
    from types import SimpleNamespace

    from hive.adapters import claude_bg

    def fake_run(argv, **kw):
        return SimpleNamespace(
            returncode=0,
            stdout='\x1b[32m[{"jobId": "abcd1234"}]\x1b[39m',
            stderr="",
        )

    monkeypatch.setattr(claude_bg.subprocess, "run", fake_run)
    assert claude_bg.list_jobs() == [{"jobId": "abcd1234"}]


def test_spawn_job_parses_colored_output(monkeypatch):
    """Regression: an ANSI-wrapped jobId polled a job that does not exist,
    so every engine-parented spawn timed out as 'never registered'."""
    from types import SimpleNamespace

    from hive.adapters import claude_bg

    def fake_run(argv, **kw):
        return SimpleNamespace(
            returncode=0,
            stdout="opus backgrounded · \x1b[36mce5de22a\x1b[39m\n",
            stderr="",
        )

    monkeypatch.setattr(claude_bg.subprocess, "run", fake_run)
    assert claude_bg.spawn_job(cwd="/tmp", name="x", prompt="hi") == "ce5de22a"


def test_success_probe_short_circuits_the_slash_confirm_window(monkeypatch, tmp_path):
    """A caller's positive oracle (the /rename registry flip) confirms the
    submit immediately — without it every successful slash burned the full
    slash-confirm window waiting for a failure shape that never came."""
    pipe = FakePipe()
    path = _transcript(tmp_path, [])  # no confirming record ever appears
    _wire(monkeypatch, pipe, screens=["> /rename comb.a"], transcript=path)

    result = claude_bg.type_into_job(
        "cafe1234", "/rename comb.a", success_probe=lambda: True
    )

    assert result.ok and result.confirmed == "probe"


def test_ensure_job_named_confirms_via_registry_flip(monkeypatch, tmp_path):
    pipe = FakePipe()
    path = _transcript(tmp_path, [])
    _wire(monkeypatch, pipe, screens=["> /rename comb.a"], transcript=path)
    names = iter(["old-name", "comb.a"])  # pre-check, then probe

    def fake_engine(_jid):
        from types import SimpleNamespace
        return SimpleNamespace(name=next(names), pid=1, job_id="cafe1234",
                               session_id="s", socket_path="/tmp/x", cwd="",
                               status="idle", waiting_for="", status_updated_at=0.0)

    monkeypatch.setattr(claude_bg, "engine_session_for_job", fake_engine)
    assert claude_bg.ensure_job_named("cafe1234", "comb.a") is True
