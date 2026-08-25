"""Agent: an agent CLI instance running in a tmux pane."""

from __future__ import annotations

import os
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import draft_guard
from . import tmux
from .agent_cli import resolve_session_id_for_pane

AGENT_STARTUP_TIMEOUT = 30
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."

SUPPORTED_CLIS: tuple[str, ...] = ("claude", "codex", "grok")


def _shell_escape(s: str) -> str:
    """Escape a string for safe shell use."""
    return "'" + s.replace("'", "'\\''") + "'"


def _resolve_session_id_from_runtime(pane_id: str = "") -> str | None:
    resolved_pane = pane_id or tmux.get_current_pane_id() or ""
    if not resolved_pane:
        return None
    return resolve_session_id_for_pane(resolved_pane)


def detect_current_session_id(cwd: str, model: str = "", pane_id: str = "") -> str | None:
    """Best-effort lookup for the current pane's agent session ID."""
    return _resolve_session_id_from_runtime(pane_id)


class DeliveryError(RuntimeError):
    """A native transport (codex daemon / grok leader / claude inbox) did not accept the
    message. Normal hive delivery never falls back to keystrokes; callers
    surface this as an explicit submit failure (injectStatus=failed)."""


def _submit_interactive_text(pane_id: str, text: str, cli: str) -> None:
    """Submit text to an interactive agent TUI, preserving any pending draft."""
    profile_name = _resolve_profile_name(pane_id, cli)
    if profile_name == "claude":
        from .adapters import claude_bg, claude_view

        job_id = claude_bg.job_id_for_pane(pane_id)
        if job_id:
            # A claude member's keyboard is the job, not the pane: hive pipes
            # the keystrokes into `claude attach <jobId>` itself. Nothing here
            # touches tmux — the pane's viewer is a screen, and what the human
            # has it showing (another session, the panel list, nothing at all)
            # cannot misroute or block a delivery.
            result = claude_bg.type_into_job(job_id, text)
            if not result.ok:
                raise RuntimeError(f"claude job {job_id} did not take the text: {result.why}")
            return
        # No job record: an interactive claude TUI on the pane tty, typed at
        # through tmux like any other CLI. Refuse rather than type into the
        # pane shell when that TUI is not running — or into an attach viewer,
        # whose composer belongs to whatever session it is showing.
        if claude_view.interactive_claude_pid(pane_id) is None:
            raise RuntimeError(
                f"no interactive claude process on pane {pane_id} to receive keystrokes"
            )

    if tmux.is_pane_in_mode(pane_id):
        tmux.cancel_pane_mode(pane_id)
        time.sleep(0.05)

    buffer_name = _save_and_clear_draft(pane_id, profile_name)

    tmux.send_keys(pane_id, text, enter=False)
    time.sleep(0.05)
    tmux.send_key(pane_id, "Enter")

    if buffer_name:
        _restore_draft(pane_id, profile_name, buffer_name)


def _save_and_clear_draft(pane_id: str, profile_name: str) -> str:
    """Best-effort: if a draft exists, save it to a tmux buffer and clear input.

    Returns the buffer name to restore later, or '' when no draft / on any error.
    """
    if not draft_guard.supported_profile(profile_name):
        return ""
    try:
        draft_text = draft_guard.parse_draft(pane_id, profile_name)
        if not draft_text:
            return ""
        buffer_name = f"hive_draft_{pane_id.replace('%', '')}"
        tmux.load_buffer(buffer_name, draft_text)
        draft_guard.clear_input(pane_id, profile_name)
        draft_guard.wait_input_empty(pane_id, profile_name, timeout=1.0)
        return buffer_name
    except Exception:
        return ""


def _restore_draft(pane_id: str, profile_name: str, buffer_name: str) -> None:
    try:
        draft_guard.wait_input_empty(pane_id, profile_name, timeout=2.0)
        tmux.paste_buffer(buffer_name, pane_id, bracketed=True)
    finally:
        tmux.delete_buffer(buffer_name)


def _resolve_profile_name(pane_id: str, cli: str) -> str:
    """Prefer runtime detection; fall back to the declared cli."""
    try:
        from .agent_cli import detect_profile_for_pane, get_profile
    except Exception:
        return cli
    profile = detect_profile_for_pane(pane_id)
    if profile is None and cli:
        profile = get_profile(cli)
    if profile is not None:
        return getattr(profile, "name", cli)
    return cli


def _wait_codex_attached(
    pane_id: str,
    *,
    timeout: float = AGENT_STARTUP_TIMEOUT,
    interval: float = 0.5,
) -> bool:
    """Wait for the codex TUI process to appear on the pane's TTY.

    The pane's thread identity is minted (and recorded) before the launch
    command runs, so readiness is just the TUI being up — process evidence,
    no screen scraping. Best-effort like the banner wait it replaces: a
    timeout is not fatal.
    """
    from .agent_cli import detect_cli_process_for_pane

    deadline = time.monotonic() + timeout
    while True:
        try:
            profile = detect_cli_process_for_pane(pane_id)
            if profile is not None and profile.name == "codex":
                return True
        except Exception:
            pass
        if time.monotonic() >= deadline:
            return False
        time.sleep(interval)


def _wait_grok_session_ready(
    pane_id: str,
    session_id: str,
    *,
    timeout: float = AGENT_STARTUP_TIMEOUT,
    interval: float = 0.5,
) -> bool:
    """Wait for the grok TUI to materialize the session hive minted for it.

    ``--session-id`` is honoured at startup: grok creates
    ``$GROK_HOME/sessions/<quoted cwd>/<sid>/`` before the first prompt, so that
    directory appearing is the readiness signal — no screen scraping. The cwd
    segment is grok's own encoding of the pane cwd, so the pane's session is
    matched by id under any of them. On resume the directory already exists, so
    the pane's live grok process is required too. Best-effort like the codex
    thread wait: a timeout is not fatal.
    """
    from .adapters import grok_leader
    from .agent_cli import detect_cli_process_for_pane

    deadline = time.monotonic() + timeout
    while True:
        try:
            if any(grok_leader.grok_home().glob(f"sessions/*/{session_id}")):
                profile = detect_cli_process_for_pane(pane_id)
                if profile is not None and profile.name == "grok":
                    return True
        except OSError:
            pass
        if time.monotonic() >= deadline:
            return False
        time.sleep(interval)


@dataclass
class Agent:
    name: str
    team_name: str
    pane_id: str
    model: str = ""
    prompt: str = ""
    cwd: str = field(default_factory=os.getcwd)
    session_id: str | None = None
    spawned_at: float = field(default_factory=time.time)
    cli: str = "claude"

    # --- Lifecycle ---

    @classmethod
    def spawn(
        cls,
        name: str,
        team_name: str,
        target_pane: str,
        model: str = "",
        prompt: str = "",
        cwd: str = "",
        session_id: str | None = None,
        is_first: bool = False,
        split_horizontal: bool = True,
        split_size: str | None = None,
        split_window: bool = True,
        skill: str = "hive",
        extra_env: dict[str, str] | None = None,
        cli: str = "claude",
        workspace: str = "",
        session_mode: str = "fork",
    ) -> Agent:
        """Spawn an agent CLI (claude/codex/grok) in a tmux pane.

        If split_window is True (default), splits *target_pane* and runs the
        CLI in the new pane. If False, runs the CLI in *target_pane* itself
        (target must be a shell pane, not already running an agent).

        With a *session_id*, *session_mode* picks the semantics: ``fork``
        (default, existing behavior) branches a copy of the session; ``resume``
        continues it — claude drops ``--fork-session``, codex runs the
        daemon-native ``resume`` subcommand (a resumed team member is
        first-class, so the embedded shortcut fork uses is not allowed), grok
        drops ``--fork-session`` and keeps the resumed session's own id.
        """
        if cli not in SUPPORTED_CLIS:
            raise ValueError(f"unsupported cli '{cli}', must be one of: {', '.join(SUPPORTED_CLIS)}")
        if session_mode not in ("fork", "resume"):
            raise ValueError(f"unsupported session_mode '{session_mode}', must be fork or resume")
        cwd = cwd or os.getcwd()
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        from .agent_cli import get_profile
        profile = get_profile(cli)
        ready_text = profile.ready_text if profile else "for help"

        resolved_model = model


        if split_window:
            pane_id = tmux.split_window(target_pane, horizontal=split_horizontal, size=split_size)
        else:
            pane_id = target_pane
        tmux.set_pane_title(pane_id, f"[{name}]")
        tmux.tag_pane(pane_id, "agent", name, team_name, cli=cli)

        def _undo_pane_side_effects() -> None:
            """Give the pane back after a daemon failure: a split pane is ours
            to kill, an in-place one only loses the tags/title just written."""
            if split_window:
                tmux.kill_pane(pane_id)
            else:
                tmux.clear_pane_tags(pane_id)
                tmux.set_pane_title(pane_id, "")

        # Every CLI accepts a positional [prompt] arg (also on resume/fork).
        # Skill activation + optional user prompt are composed here, before
        # the CLI branches: a claude member's prompt goes into the bg spawn
        # itself, codex/grok pass it on the launch command line — either way
        # the CLI auto-submits at startup, bypassing TUI keystroke injection
        # entirely.
        initial_prompt = ""
        if skill and skill != "none":
            initial_prompt = profile.skill_cmd.format(name=skill) if profile else f"/{skill}"
        if prompt:
            initial_prompt = f"{initial_prompt}\n\n{prompt}" if initial_prompt else prompt
        # The launch goes through `hive <cli>`, whose parser strips any `--`
        # separator, so a prompt cannot be protected from being read as a
        # flag; refuse the one shape that would be.
        if initial_prompt.startswith("-"):
            raise ValueError("initial prompt must not start with '-'")

        # The pane runs hive's managed launcher (`hive claude` / `hive codex` /
        # `hive grok`), the same path a human's `hclaude` / `hcodex` / `hgrok`
        # takes — but invoked as the binary, not the shell function, so a spawn
        # never depends on the pane shell's rc having sourced `hive shell-init`.
        # No `exec`: the CLI runs as the pane shell's foreground child, so the
        # pane (and a usable shell) survives the CLI exiting.
        cmd_parts = ["hive", cli]
        grok_session_id = ""
        if cli == "claude":
            # A claude member is a `claude --bg` job: the engine runs on
            # claude's own supervisor, the pane only watches it through the
            # managed launcher's attach loop. The job is minted (or woken)
            # up front — like codex's thread — so the member has a durable
            # identity and a deliverable inbox before the pane even draws.
            from .adapters import claude_bg, claude_sessions

            if session_id and session_mode == "resume":
                # The member IS the job: attach wakes a parked/stopped
                # engine with the same jobId/sessionId, so resume is just
                # rebinding the pane to it.
                claude_job_id = session_id
                engine = claude_bg.ensure_engine(
                    claude_job_id, timeout=AGENT_STARTUP_TIMEOUT
                )
                if engine is None:
                    _undo_pane_side_effects()
                    raise RuntimeError(
                        f"claude job '{claude_job_id}' did not come back up "
                        "(removed from the job ledger, or the wake failed); "
                        "cannot resume this member"
                    )
                if initial_prompt:
                    # Resume carries no launch prompt; the engine's inbox is
                    # already live, so hand it over there (best-effort).
                    claude_sessions.send(
                        engine.socket_path,
                        initial_prompt,
                        sender=f"{team_name}.{name}",
                    )
            else:
                extra_args: list[str] = []
                if model:
                    extra_args.extend(["--model", model])
                if session_id:  # session_mode == "fork": branch a copy
                    extra_args.extend(["-r", session_id, "--fork-session"])
                claude_job_id = claude_bg.spawn_job(
                    cwd=cwd,
                    name=f"{team_name}.{name}",
                    prompt=initial_prompt,
                    extra_args=extra_args,
                    extra_env=extra_env,
                )
                if not claude_job_id:
                    _undo_pane_side_effects()
                    raise RuntimeError(
                        f"`claude --bg` refused to mint a job for '{name}' "
                        f"(cwd {cwd}); refusing to spawn a claude member "
                        "without a job identity (needs a Claude Code with "
                        "background sessions, 2.1.240+)"
                    )
                engine = claude_bg.wait_engine_entry(
                    claude_job_id, timeout=AGENT_STARTUP_TIMEOUT
                )
                if engine is None:
                    claude_bg.stop_job(claude_job_id)
                    _undo_pane_side_effects()
                    raise RuntimeError(
                        f"claude job '{claude_job_id}' started but its engine "
                        "never registered an inbox; claude delivery is "
                        "inbox-only, refusing to keep an undeliverable member"
                    )
            claude_bg.write_pane_job(
                pane_id, claude_job_id, engine.session_id if engine else "", cwd
            )
            # The managed launcher recognizes a jobId and runs the attach
            # watch loop (auto-reattach across engine respawns/upgrades).
            cmd_parts.extend(["--resume", _shell_escape(claude_job_id)])
        elif cli == "codex":
            cmd_parts.extend(["-c", "check_for_update_on_startup=false"])
            from .adapters import codex_app_server
            if session_id and session_mode == "fork":
                # The managed launcher forks server-side (`hive codex fork
                # <sid>` → thread/fork → resume of the fork) and records the
                # pane's thread itself; nothing to mint here.
                cmd_parts.extend(["fork", _shell_escape(session_id)])
            else:
                # Every codex member runs on the shared app-server daemon and
                # owns exactly one thread. A new member's thread is minted by
                # hive up front (thread/start + name/set flush), a resumed
                # member's thread is its recorded sessionId (== threadId), and
                # the TUI attaches with `resume <threadId>` through the
                # managed launcher (which injects --remote/--cd).
                if not codex_app_server.spawn_daemon():
                    # Codex runtime state is daemon-native only (embedded codex
                    # is unsupported), so a pane without a daemon would join the
                    # team stateless. Undo the pane side effects instead of
                    # leaving a tagged inert member behind.
                    _undo_pane_side_effects()
                    raise RuntimeError(
                        "codex shared app-server daemon failed to start; "
                        "codex runtime is daemon-only, refusing to spawn an "
                        "embedded codex team member"
                    )
                codex_app_server.ensure_dir_trusted(cwd)
                if session_id:  # session_mode == "resume"
                    codex_thread_id = session_id
                else:
                    codex_thread_id = codex_app_server.start_member_thread(
                        cwd, name=f"{team_name}.{name}", model=model,
                    )
                    if not codex_thread_id:
                        _undo_pane_side_effects()
                        raise RuntimeError(
                            f"codex app-server refused to mint a thread for "
                            f"'{name}' (cwd {cwd}); refusing to spawn a codex "
                            "member without a thread identity"
                        )
                codex_app_server.write_pane_thread(pane_id, codex_thread_id, cwd)
                cmd_parts.extend(["resume", _shell_escape(codex_thread_id)])
                # Bring the sidecar's client online now so it holds the
                # broadcast stream before the member's first turn.
                # Best-effort: a down/slow sidecar just falls back to the
                # lazy connect on the next runtime tick.
                if workspace:
                    from .sidecar import request_connect_codex
                    request_connect_codex(workspace)
        elif cli == "grok":
            from .adapters import grok_leader
            if not grok_leader.spawn_daemon(pane_id):
                # Grok runtime state lives on the per-pane leader; without one
                # the TUI would run detached from hive. Same deal as codex: give
                # the pane back rather than tag an unreachable member.
                _undo_pane_side_effects()
                raise RuntimeError(
                    f"grok leader daemon failed to start for pane {pane_id}; "
                    "grok runtime is leader-only, refusing to spawn an "
                    "unattached grok team member"
                )
            # The leader cannot say which of the cwd's sessions is this pane's,
            # so hive mints the id, hands it to the TUI and records it beside
            # the socket. A resume keeps the resumed session's own id.
            if session_id and session_mode == "resume":
                grok_session_id = session_id
            else:
                grok_session_id = str(uuid.uuid4())
                cmd_parts.extend(["--session-id", grok_session_id])
            grok_leader.write_pane_session(pane_id, grok_session_id, cwd)
        pre_cmd_parts: list[str] = []

        # claude pins model/resume/prompt at bg-spawn time and codex at
        # thread/start; only grok takes them on the launch command line.
        if cli == "grok":
            if model and not session_id:
                cmd_parts.extend(["-m", _shell_escape(model)])
            if session_id:
                # Resume/fork uses the original session's model.
                cmd_parts.extend(["--resume", _shell_escape(session_id)])
                if session_mode == "fork":
                    # `--session-id` (already on cmd_parts) names the fork.
                    cmd_parts.append("--fork-session")

        # codex/grok take the composed prompt as the launch's positional arg
        # (codex rides `resume`'s own [PROMPT] positional); claude's already
        # went into the bg spawn.
        if initial_prompt and cli != "claude":
            cmd_parts.append(_shell_escape(initial_prompt))

        env_parts: list[str] = []
        if extra_env:
            for k, v in extra_env.items():
                env_parts.append(f"{k}={_shell_escape(v)}")

        cmd = f"cd {_shell_escape(cwd)}"
        if env_parts:
            cmd = f"{cmd} && export {' '.join(env_parts)}"
        if pre_cmd_parts:
            cmd = f"{cmd} && {' && '.join(pre_cmd_parts)}"
        # After the CLI exits the pane keeps its shell, so print the cd-ready
        # resume hint there — the same tail `hclaude` / `hcodex` run.
        cmd = f"{cmd} && {' '.join(cmd_parts)}; hive resume-hint {cli} 2>/dev/null || true"
        tmux.send_keys(pane_id, cmd)

        agent = cls(
            name=name,
            team_name=team_name,
            pane_id=pane_id,
            model=model,
            prompt=prompt,
            cwd=cwd,
            session_id=session_id,
            cli=cli,
        )

        # Readiness comes from runtime signals, not screen text: the claude
        # engine's registry entry (proven before the pane command was even
        # typed), the codex TUI process on the pane TTY, and the minted
        # session directory (grok) can only appear once the agent is actually
        # up.
        if cli == "claude":
            pass  # engine entry proven pre-launch; the pane only watches
        elif cli == "codex":
            _wait_codex_attached(pane_id)
        elif cli == "grok":
            # The 2nd client can only load a session the TUI has opened, so the
            # connect follows readiness instead of racing it.
            if _wait_grok_session_ready(pane_id, grok_session_id) and workspace:
                from .sidecar import request_connect_grok
                request_connect_grok(workspace, pane_id)
        elif tmux.wait_for_text(pane_id, ready_text, timeout=AGENT_STARTUP_TIMEOUT):
            time.sleep(1)


        return agent

    # --- Control ---

    def send(self, text: str) -> str:
        """Send a prompt to the agent; return the accepted-transport class.

        Delivery is native-transport-only: codex goes through the shared
        daemon's ``turn/start`` RPC on the member's recorded thread, grok
        through its per-pane leader's
        ``session/prompt``, claude through its session's own inbox socket. None
        of them touches the composer, and there is no keystroke fallback on any
        failure — a transport that did not accept the message raises
        :class:`DeliveryError` (callers surface it as an explicit submit
        failure). The returned classification names which transport boundary
        was crossed (``turnStartAccepted`` / ``sessionPromptQueued`` /
        ``udsWriteAccepted``); none of them proves the agent processed
        the message — that final confirmation only ever comes from the
        target's transcript.
        """
        # A claude member's engine is not on the pane TTY at all: the pane's
        # job record is its address, and a parked engine (supervisor idles
        # jobs after ~1h) is woken in-line. The record is only trusted when
        # the pane shows no *other* live CLI — a recycled pane id running
        # codex must never route into a stale claude record.
        probe = None
        try:
            from .agent_cli import detect_cli_process_for_pane

            probe = detect_cli_process_for_pane(self.pane_id)
        except Exception:
            probe = None
        profile_name = probe.name if probe is not None else ""
        if profile_name in ("", "claude"):
            from .adapters import claude_bg, claude_sessions

            job_id = claude_bg.job_id_for_pane(self.pane_id)
            if job_id:
                engine = claude_bg.engine_session_for_job(job_id)
                if engine is None and claude_bg.job_row(job_id) is not None:
                    # Asleep, not dead: the job ledger still lists it, and a
                    # tty-less attach revives the engine (same jobId and
                    # sessionId, fresh pid) — then re-read its new entry.
                    engine = claude_bg.ensure_engine(job_id)
                if engine is None:
                    raise DeliveryError(
                        f"claude job '{job_id}' for pane {self.pane_id} is "
                        "gone (removed from the job ledger, or the wake "
                        "failed); the message stays on the bus"
                    )
                accepted = claude_sessions.send(
                    engine.socket_path, text, sender=f"{self.team_name}.{self.name}"
                )
                if accepted == claude_sessions.WRITE_TIMED_OUT:
                    raise DeliveryError(
                        f"claude job '{job_id}' (pane {self.pane_id}) accepted "
                        "the connection but did not drain the message in time"
                    )
                if accepted is None:
                    raise DeliveryError(
                        f"claude job '{job_id}' (pane {self.pane_id}) is not "
                        "listening on its inbox; the message stays on the bus"
                    )
                return accepted
        if probe is None:
            raise DeliveryError(
                f"no live CLI process on pane {self.pane_id} (cli_exited): "
                "refusing native transport to a retained shell"
            )
        if profile_name == "codex":
            from .adapters import codex_app_server

            accepted = codex_app_server.send_to_pane(self.pane_id, text)
            if accepted is None:
                raise DeliveryError(
                    f"codex pane {self.pane_id} did not accept the turn "
                    "(no recorded thread, daemon down, RPC error, or "
                    "connection failure)"
                )
            return accepted
        if profile_name == "grok":
            from .adapters import grok_leader

            accepted = grok_leader.send_to_pane(self.pane_id, text)
            if accepted is None:
                raise DeliveryError(
                    f"grok pane {self.pane_id} did not accept the prompt "
                    "(no leader/session, RPC error, or connection failure)"
                )
            return accepted
        if profile_name == "claude":
            raise DeliveryError(
                f"claude pane {self.pane_id} has no bg job record; a hive "
                "claude member runs as a background job (relaunch it with "
                "`hive claude`) — hive does not deliver to a bare claude TUI"
            )
        raise DeliveryError(
            f"pane {self.pane_id} runs no supported agent CLI "
            f"(profile={profile_name or 'unknown'}); hive delivers over "
            "native transports only"
        )

    def interrupt(self) -> None:
        """Press Escape to interrupt.

        A claude member's Escape rides the same pipe as its text — addressed
        to the job, so it interrupts *that* engine's turn whatever the pane's
        viewer happens to be showing.
        """
        if self.cli == "claude":
            from .adapters import claude_bg

            job_id = claude_bg.job_id_for_pane(self.pane_id)
            if job_id:
                result = claude_bg.interrupt_job(job_id)
                if not result.ok:
                    raise RuntimeError(f"claude job {job_id} was not interrupted: {result.why}")
                return
        tmux.send_key(self.pane_id, "Escape")

    def capture(self, lines: int = 50) -> str:
        """Capture pane output."""
        return tmux.capture_pane(self.pane_id, lines)

    def is_alive(self) -> bool:
        return tmux.is_pane_alive(self.pane_id)

    def shutdown(self) -> None:
        """Send Ctrl+C twice then exit."""
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_keys(self.pane_id, "exit")

    def kill(self) -> None:
        """Force kill the pane — and, for a claude member, park its engine.

        The engine lives on claude's supervisor, not in the pane, so killing
        the pane alone would leave an orphan job running headless. ``claude
        stop`` parks it: the job stays in the ledger and ``hive resume``
        can still wake it.
        """
        if self.cli == "claude":
            from .adapters import claude_bg

            job_id = claude_bg.job_id_for_pane(self.pane_id)
            if job_id:
                claude_bg.stop_job(job_id)
            claude_bg.clear_pane_job(self.pane_id)
        tmux.kill_pane(self.pane_id)

    # --- Serialization ---

    def to_dict(self) -> dict:
        return {
            "agentId": f"{self.name}@{self.team_name}",
            "name": self.name,
            "model": self.model,
            "prompt": self.prompt,
            "cwd": self.cwd,
            "tmuxPaneId": self.pane_id,
            "sessionId": self.session_id,
            "spawnedAt": self.spawned_at,
            "isActive": self.is_alive(),
            "cli": self.cli,
        }
