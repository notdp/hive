"""Agent: an agent CLI instance running in a tmux pane."""

from __future__ import annotations

import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import draft_guard
from . import skill_sync
from . import tmux
from .agent_cli import resolve_session_id_for_pane

AGENT_STARTUP_TIMEOUT = 30
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."

CLI_BINS: dict[str, str] = {
    "claude": "claude",
    "codex": "codex",
}


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
    """A native transport (codex daemon / claude channel) did not accept the
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


_CHANNEL_NOTICE_GRACE = 4.0


def _drive_claude_channel_startup(pane_id: str, ready_text: str) -> bool:
    """Answer Claude's one-shot startup prompts (folder trust, MCP-server
    consent for the project's own servers -- each defaults to the safe first
    option) until the channel server reports ready.

    Spawn-time startup-consent driving only -- never message delivery. The
    success signal is the pane's ready marker, written by the channel server
    itself once its socket is listening (the plugin-provided channel loads
    with no consent dialog, so a running server means the channel is live).
    Returns ``False`` when Claude reached ready (or the deadline)
    without the marker appearing -- the pane cannot receive hive messages and
    spawn must fail. The caller cleared any stale marker before launch.
    """
    from .adapters import claude_channel

    prompts = (
        "trust this folder",
        "New MCP server found",
        "Use this MCP server",
    )
    deadline = time.time() + AGENT_STARTUP_TIMEOUT
    ready_at: float | None = None
    while time.time() < deadline:
        if claude_channel.is_ready(pane_id):
            return True
        screen = tmux.capture_pane(pane_id, lines=80)
        if any(p in screen for p in prompts):
            tmux.send_key(pane_id, "Enter")
            ready_at = None
            time.sleep(0.6)
            continue
        if ready_text in screen:
            if ready_at is None:
                ready_at = time.time()
            elif time.time() - ready_at > _CHANNEL_NOTICE_GRACE:
                return False  # TUI is up but the channel server never bound
        time.sleep(0.4)
    return claude_channel.is_ready(pane_id)


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
        """Spawn an agent CLI (claude/codex) in a tmux pane.

        If split_window is True (default), splits *target_pane* and runs the
        CLI in the new pane. If False, runs the CLI in *target_pane* itself
        (target must be a shell pane, not already running an agent).

        With a *session_id*, *session_mode* picks the semantics: ``fork``
        (default, existing behavior) branches a copy of the session; ``resume``
        continues it — claude drops ``--fork-session``, codex runs the
        daemon-native ``resume`` subcommand (a resumed team member is
        first-class, so the embedded shortcut fork uses is not allowed).
        """
        if cli not in CLI_BINS:
            raise ValueError(f"unsupported cli '{cli}', must be one of: {', '.join(CLI_BINS)}")
        if session_mode not in ("fork", "resume"):
            raise ValueError(f"unsupported session_mode '{session_mode}', must be fork or resume")
        cwd = cwd or os.getcwd()
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        from .agent_cli import get_profile
        profile = get_profile(cli)
        ready_text = profile.ready_text if profile else "for help"

        resolved_model = model

        # Every claude session (fresh, resume, fork — each is a new process
        # picking up the launch flags) registers a per-pane MCP "channel" so
        # hive can push <HIVE> messages over a socket. Delivery is
        # channel-only: when the channel config cannot be written, the pane
        # could never receive messages, so spawn fails before a pane is even
        # created instead of leaving an undeliverable agent behind.
        channel_flags: list[str] = []
        if cli == "claude":
            from .adapters import claude_channel
            channel_flags = claude_channel.prepare_pane(cwd)
            if not channel_flags:
                raise RuntimeError(
                    f"claude channel could not be registered in {cwd} "
                    "(see [hive-channel] stderr for the reason); claude "
                    "delivery is channel-only, refusing to spawn an "
                    "undeliverable pane"
                )

        if split_window:
            pane_id = tmux.split_window(target_pane, horizontal=split_horizontal, size=split_size)
        else:
            pane_id = target_pane
        tmux.set_pane_title(pane_id, f"[{name}]")
        tmux.tag_pane(pane_id, "agent", name, team_name, cli=cli)

        bin_path = CLI_BINS[cli]
        cmd_parts = ["exec", _shell_escape(bin_path)]
        codex_daemon_native = False
        if cli == "codex":
            cmd_parts.extend(["-c", "check_for_update_on_startup=false"])
            # A new or resumed codex session runs against a per-pane app-server
            # daemon: hive starts the daemon (injecting this pane's TMUX_PANE so
            # shell tools keep the right identity, sharing the real CODEX_HOME)
            # and the TUI joins it via --remote. cwd is passed explicitly
            # because Remote workspace mode drops config.cwd. Only the fork
            # handoff shortcut stays embedded (below).
            if not session_id or session_mode == "resume":
                from .adapters import codex_app_server
                if not codex_app_server.spawn_daemon(pane_id):
                    # Codex runtime state is daemon-native only (embedded codex
                    # is unsupported), so a pane without a daemon would join the
                    # team stateless. Undo the pane side effects instead of
                    # leaving a tagged inert member behind.
                    if split_window:
                        tmux.kill_pane(pane_id)
                    else:
                        tmux.clear_pane_tags(pane_id)
                        tmux.set_pane_title(pane_id, "")
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
        if channel_flags:
            cmd_parts.extend(channel_flags)
        pre_cmd_parts: list[str] = []

        if model and not session_id:
            if cli == "claude":
                cmd_parts.extend(["--model", _shell_escape(model)])
            elif cli == "codex":
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
                    cmd_parts = ["exec", _shell_escape(bin_path), "-c", "check_for_update_on_startup=false", "fork", _shell_escape(session_id)]

        # Both CLIs accept a positional [prompt] arg (also on resume/fork).
        # Pass skill activation + optional user prompt here so the CLI
        # auto-submits at startup, bypassing TUI keystroke injection entirely
        # (avoids the codex picker race and any analogous races for claude).
        initial_prompt = ""
        if skill and skill != "none":
            initial_prompt = profile.skill_cmd.format(name=skill) if profile else f"/{skill}"
        if prompt:
            initial_prompt = f"{initial_prompt}\n\n{prompt}" if initial_prompt else prompt
        if initial_prompt:
            if cli == "claude":
                # Claude's channel flags are variadic: without a `--`
                # separator the parser consumes the positional prompt as a
                # flag value and aborts launch.
                cmd_parts.append("--")
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
        cmd = f"{cmd} && {' '.join(cmd_parts)}"
        if channel_flags:
            # A stale marker from a previous claude in this pane id must not
            # be mistaken for the new server's readiness.
            from .adapters import claude_channel
            claude_channel.clear_ready(pane_id)
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

        if channel_flags:
            if not _drive_claude_channel_startup(pane_id, ready_text):
                from .adapters import claude_channel
                claude_channel.clear_ready(pane_id)
                if split_window:
                    tmux.kill_pane(pane_id)
                raise RuntimeError(
                    f"claude started in pane {pane_id} but never registered "
                    "the hive channel; claude delivery is channel-only, "
                    "refusing to keep an undeliverable pane (channels "
                    "verified on Claude Code 2.1.198 with Anthropic auth; "
                    "older versions or Bedrock/Vertex may not support them)"
                )

        # Readiness comes from runtime signals, not screen text: the channel
        # marker (claude, proven above) and the app-server thread (codex
        # daemon) can only appear once the agent is actually up. A resumed
        # claude session never renders the welcome banner, so a banner wait
        # here ate its full timeout on every resume. Only the embedded codex
        # fork (no daemon) has nothing better than the banner.
        if channel_flags:
            pass
        elif codex_daemon_native:
            _wait_codex_thread_ready(pane_id)
        elif tmux.wait_for_text(pane_id, ready_text, timeout=AGENT_STARTUP_TIMEOUT):
            time.sleep(1)

        # Skill + user prompt were embedded in the [prompt] arg above.
        if skill == "hive":
            skill_sync.maybe_warn_hive_skill_drift(cli)

        return agent

    # --- Control ---

    def send(self, text: str) -> str:
        """Send a prompt to the agent; return the accepted-transport class.

        Delivery is native-transport-only: codex goes through the per-pane
        daemon's ``turn/start`` RPC, claude through its per-pane MCP channel
        socket. Neither touches the composer, and there is no keystroke
        fallback on any failure — a transport that did not accept the message
        raises :class:`DeliveryError` (callers surface it as an explicit
        submit failure). The returned classification names which transport
        boundary was crossed (``turnStartAccepted`` / ``mcpWriteAccepted`` /
        ``legacySocketAccepted``); none of them proves the agent processed
        the message — that final confirmation only ever comes from the
        target's transcript.
        """
        profile_name = _resolve_profile_name(self.pane_id, self.cli)
        if profile_name == "codex":
            from .adapters import codex_app_server

            accepted = codex_app_server.send_to_pane(self.pane_id, text)
            if accepted is None:
                raise DeliveryError(
                    f"codex pane {self.pane_id} did not accept the turn "
                    "(no daemon/thread, RPC error, or connection failure)"
                )
            return accepted
        if profile_name == "claude":
            from .adapters import claude_channel

            accepted = claude_channel.send_to_pane(self.pane_id, text)
            if accepted is None:
                raise DeliveryError(
                    f"claude pane {self.pane_id} channel transport failed "
                    "(marker/socket missing, write error, or no MCP-write "
                    "receipt); claude delivery is channel-only"
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
