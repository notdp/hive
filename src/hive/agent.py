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
    if tmux.is_pane_in_mode(pane_id):
        tmux.cancel_pane_mode(pane_id)
        time.sleep(0.05)

    profile_name = _resolve_profile_name(pane_id, cli)
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


_INBOX_NOTICE_GRACE = 6.0
# The inbox registers at process start — BEFORE the folder-trust / MCP-consent
# dialogs — so registration alone must not end the drive: a modal could still
# be blocking the TUI. After the inbox is proven, the loop keeps answering
# prompts until the banner shows, or the screen stays prompt-free this long.
_PROMPT_SETTLE = 1.5


def _drive_claude_startup(pane_id: str, ready_text: str) -> bool:
    """Answer Claude's one-shot startup prompts (folder trust, MCP-server
    consent for the project's own servers -- each defaults to the safe first
    option) until the session has bound its cross-session inbox AND the TUI is
    past its startup dialogs.

    Spawn-time startup-consent driving only -- never message delivery. The
    delivery signal is the session's registry entry appearing for the pane's
    claude process: claude binds the inbox itself, so a registered session
    means hive can deliver. Returns ``False`` when Claude reached ready (or
    the deadline) without an inbox -- the pane cannot receive hive messages
    and spawn must fail.
    """
    from .adapters import claude_sessions
    from .agent_cli import claude_pid_for_pane

    prompts = (
        "trust this folder",
        "New MCP server found",
        "Use this MCP server",
    )
    deadline = time.time() + AGENT_STARTUP_TIMEOUT
    inbox_ok = False
    ready_at: float | None = None
    settled_at: float | None = None
    while time.time() < deadline:
        screen = tmux.capture_pane(pane_id, lines=80)
        if any(p in screen for p in prompts):
            tmux.send_key(pane_id, "Enter")
            ready_at = None
            settled_at = None
            time.sleep(0.6)
            continue
        if not inbox_ok:
            pid = claude_pid_for_pane(pane_id)
            if pid and claude_sessions.session_for_pid(pid) is not None:
                inbox_ok = True
        if inbox_ok:
            if ready_text in screen:
                return True  # banner up, no dialog on screen
            # A resumed session never renders the banner: a prompt-free screen
            # holding steady is the best remaining "past the dialogs" signal.
            if settled_at is None:
                settled_at = time.time()
            elif time.time() - settled_at > _PROMPT_SETTLE:
                return True
        elif ready_text in screen:
            if ready_at is None:
                ready_at = time.time()
            elif time.time() - ready_at > _INBOX_NOTICE_GRACE:
                return False  # TUI is up but the session never bound an inbox
        time.sleep(0.4)
    return inbox_ok


def _wait_codex_thread_ready(
    pane_id: str,
    *,
    timeout: float = AGENT_STARTUP_TIMEOUT,
    interval: float = 0.5,
) -> bool:
    """Wait for the pane's app-server daemon to expose a live thread runtime.

    The TUI joining the daemon (fresh or resume) creates/attaches its thread;
    that runtime appearing is the readiness signal — no screen scraping. Polls
    through the persistent client pool (no per-round reconnect). Best-effort
    like the banner wait it replaces: a timeout is not fatal.
    """
    from .adapters import codex_app_server

    deadline = time.monotonic() + timeout
    while True:
        try:
            if codex_app_server.runtime_for_pane(pane_id) is not None:
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

        # The pane runs hive's managed launcher (`hive claude` / `hive codex` /
        # `hive grok`), the same path a human's `hclaude` / `hcodex` / `hgrok`
        # takes — but invoked as the binary, not the shell function, so a spawn
        # never depends on the pane shell's rc having sourced `hive shell-init`.
        # No `exec`: the CLI runs as the pane shell's foreground child, so the
        # pane (and a usable shell) survives the CLI exiting.
        cmd_parts = ["hive", cli]
        codex_daemon_native = False
        grok_session_id = ""
        if cli == "codex":
            cmd_parts.extend(["-c", "check_for_update_on_startup=false"])
            # A new or resumed codex session runs against a per-pane app-server
            # daemon: hive starts the daemon (injecting this pane's TMUX_PANE so
            # shell tools keep the right identity, sharing the real CODEX_HOME)
            # and the TUI joins it via --remote. cwd is passed explicitly
            # because Remote workspace mode drops config.cwd. The fork shortcut
            # (below) also goes through `hive codex`, which binds a daemon and
            # injects --remote itself (verified on codex 0.147.0: the forked
            # thread is created on and held by that daemon); no production
            # path forks a codex member yet — the team-fork gate refuses it.
            if not session_id or session_mode == "resume":
                from .adapters import codex_app_server
                if not codex_app_server.spawn_daemon(pane_id):
                    # Codex runtime state is daemon-native only (embedded codex
                    # is unsupported), so a pane without a daemon would join the
                    # team stateless. Undo the pane side effects instead of
                    # leaving a tagged inert member behind.
                    _undo_pane_side_effects()
                    raise RuntimeError(
                        f"codex app-server daemon failed to start for pane {pane_id}; "
                        "codex runtime is daemon-only, refusing to spawn an "
                        "embedded codex team member"
                    )
                codex_daemon_native = True
                sock = codex_app_server.pane_socket_path(pane_id)
                cmd_parts.extend([
                    "--remote", _shell_escape(f"unix://{sock}"),
                    "--cd", _shell_escape(cwd),
                ])
                # Bring hive's 2nd client online now — before codex (started
                # by send_keys at the end of this method) creates its thread
                # — so the sidecar receives the thread/started + status
                # broadcast live instead of late-joining and resuming.
                # Best-effort: a down/slow sidecar just falls back to the
                # lazy connect on the next runtime tick.
                if workspace:
                    from .sidecar import request_connect_codex
                    request_connect_codex(workspace, pane_id)
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

        if model and not session_id:
            if cli == "claude":
                cmd_parts.extend(["--model", _shell_escape(model)])
            elif cli in ("codex", "grok"):
                cmd_parts.extend(["-m", _shell_escape(model)])

        # Resume/fork uses the original session's model; no --model flag needed.
        if session_id:
            if cli == "claude":
                cmd_parts.extend(["-r", _shell_escape(session_id)])
                if session_mode == "fork":
                    cmd_parts.append("--fork-session")
            elif cli == "codex":
                if session_mode == "resume":
                    # Daemon flags (--remote/--cd) are already on cmd_parts.
                    cmd_parts.extend(["resume", _shell_escape(session_id)])
                else:
                    cmd_parts = ["hive", "codex", "-c", "check_for_update_on_startup=false", "fork", _shell_escape(session_id)]
            elif cli == "grok":
                cmd_parts.extend(["--resume", _shell_escape(session_id)])
                if session_mode == "fork":
                    # `--session-id` (already on cmd_parts) names the fork.
                    cmd_parts.append("--fork-session")

        # Every CLI accepts a positional [prompt] arg (also on resume/fork).
        # Pass skill activation + optional user prompt here so the CLI
        # auto-submits at startup, bypassing TUI keystroke injection entirely
        # (avoids the codex picker race and any analogous races for claude).
        initial_prompt = ""
        if skill and skill != "none":
            initial_prompt = profile.skill_cmd.format(name=skill) if profile else f"/{skill}"
        if prompt:
            initial_prompt = f"{initial_prompt}\n\n{prompt}" if initial_prompt else prompt
        if initial_prompt:
            # The launch goes through `hive <cli>`, whose parser strips any
            # `--` separator, so a prompt cannot be protected from being read
            # as a flag; refuse the one shape that would be.
            if initial_prompt.startswith("-"):
                raise ValueError("initial prompt must not start with '-'")
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

        if cli == "claude":
            if not _drive_claude_startup(pane_id, ready_text):
                if split_window:
                    tmux.kill_pane(pane_id)
                raise RuntimeError(
                    f"claude started in pane {pane_id} but never bound a "
                    "cross-session inbox; claude delivery is inbox-only, "
                    "refusing to keep an undeliverable pane (needs Claude "
                    "Code >= 2.1.224, and messaging stays off when "
                    "DISABLE_TELEMETRY / DO_NOT_TRACK / DISABLE_GROWTHBOOK / "
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC disables "
                    "feature-flag evaluation; a held member also needs "
                    "crossSessionInbound left at its default or 'accept')"
                )

        # Readiness comes from runtime signals, not screen text: the inbox
        # registration (claude, proven above), the app-server thread (codex
        # daemon) and the minted session directory (grok) can only appear once
        # the agent is actually up. A resumed claude session never renders the
        # welcome banner, so a banner wait here ate its full timeout on every
        # resume. Only the embedded codex fork (no daemon) has nothing better
        # than the banner.
        if cli == "claude":
            pass  # inbox registration proven above
        elif codex_daemon_native:
            _wait_codex_thread_ready(pane_id)
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

        Delivery is native-transport-only: codex goes through the per-pane
        daemon's ``turn/start`` RPC, grok through its per-pane leader's
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
        # Native transports require a live CLI process on the pane TTY. A
        # retained shell can carry a stale title, the declared cli tag, and
        # (for codex) a surviving per-pane daemon with an open thread — none
        # of that may route a message into a pane nobody is watching.
        probe = None
        try:
            from .agent_cli import detect_cli_process_for_pane

            probe = detect_cli_process_for_pane(self.pane_id)
        except Exception:
            probe = None
        if probe is None:
            raise DeliveryError(
                f"no live CLI process on pane {self.pane_id} (cli_exited): "
                "refusing native transport to a retained shell"
            )
        profile_name = probe.name
        if profile_name == "codex":
            from .adapters import codex_app_server

            accepted = codex_app_server.send_to_pane(self.pane_id, text)
            if accepted is None:
                raise DeliveryError(
                    f"codex pane {self.pane_id} did not accept the turn "
                    "(no daemon/thread, RPC error, or connection failure)"
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
            from .adapters import claude_sessions
            from .agent_cli import claude_pid_for_pane

            session = claude_sessions.session_for_pid(claude_pid_for_pane(self.pane_id))
            if session is None:
                raise DeliveryError(
                    f"claude pane {self.pane_id} has no cross-session inbox "
                    "(claude < 2.1.224, messaging disabled by env, or the "
                    "session is still starting); claude delivery is inbox-only"
                )
            accepted = claude_sessions.send(
                session.socket_path, text, sender=f"{self.team_name}.{self.name}"
            )
            if accepted == claude_sessions.WRITE_TIMED_OUT:
                raise DeliveryError(
                    f"claude pane {self.pane_id} (session {session.name}) accepted "
                    "the connection but did not drain the message in time"
                )
            if accepted is None:
                raise DeliveryError(
                    f"claude pane {self.pane_id} (session {session.name}) is not "
                    "listening on its inbox; the message stays on the bus"
                )
            return accepted
        raise DeliveryError(
            f"pane {self.pane_id} runs no supported agent CLI "
            f"(profile={profile_name or 'unknown'}); hive delivers over "
            "native transports only"
        )

    def interrupt(self) -> None:
        """Press Escape to interrupt."""
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
        """Force kill the pane."""
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
