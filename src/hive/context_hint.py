"""Push hint when an agent's context size approaches the compact threshold.

Hive runtime watches transcript token usage per pane and surfaces it to
agents via two channels:

- A ``hint`` field appended to hive command stdout JSON.
- A ``<HIVE-HINT>`` block appended after the ``<HIVE>`` envelope on
  delivery.

Both channels are best-effort: if no context snapshot is available, the
hint is silently skipped. Agents act on the hint only at task-boundary
moments, per ``skills/hive/SKILL.md``.
"""

from __future__ import annotations

from typing import Any


CONTEXT_HINT_THRESHOLD = 400_000

# Per skills/hive/SKILL.md: first version is Claude-only. Codex hosts run
# their own auto-compact and must not receive this nag. Other adapters
# (droid) currently emit no context snapshot and therefore never reach
# the threshold check.
_SUPPORTED_CONTEXT_SOURCES = frozenset({"claude_assistant_usage"})


def _hint_text(tokens: int, window: int | None) -> str:
    window_part = f"/{window}" if isinstance(window, int) and window > 0 else ""
    return (
        f"context: {tokens}{window_part} tokens. "
        f"when the current big task winds down, run: hive compact"
    )


def _extract_context(member_runtime: Any) -> tuple[int, int | None] | None:
    if not isinstance(member_runtime, dict):
        return None
    ctx = member_runtime.get("context")
    if not isinstance(ctx, dict):
        return None
    if ctx.get("source") not in _SUPPORTED_CONTEXT_SOURCES:
        return None
    tokens = ctx.get("tokens")
    if not isinstance(tokens, int) or tokens <= CONTEXT_HINT_THRESHOLD:
        return None
    window = ctx.get("window")
    return tokens, window if isinstance(window, int) else None


def maybe_attach_hint(
    payload: dict[str, Any],
    *,
    self_name: str,
    team_runtime: dict[str, Any] | None,
) -> None:
    """Mutate *payload* with a ``hint`` field when self context > threshold.

    *team_runtime* is the dict returned by ``sidecar.request_team_runtime``;
    when it is ``None`` or has no context for *self_name* the payload is
    left untouched.
    """
    if not self_name or not isinstance(team_runtime, dict):
        return
    members = team_runtime.get("members")
    if not isinstance(members, dict):
        return
    extracted = _extract_context(members.get(self_name))
    if extracted is None:
        return
    tokens, window = extracted
    payload["compactHint"] = _hint_text(tokens, window)


def hint_block_for(
    *,
    self_name: str,
    team_runtime: dict[str, Any] | None,
) -> str:
    """Return a ``<HIVE-HINT>...</HIVE-HINT>`` string, or empty when no hint.

    Used when delivering a send: the receiver's context is checked, and a
    standalone block is appended after the ``<HIVE>`` envelope.
    """
    if not self_name or not isinstance(team_runtime, dict):
        return ""
    members = team_runtime.get("members")
    if not isinstance(members, dict):
        return ""
    extracted = _extract_context(members.get(self_name))
    if extracted is None:
        return ""
    tokens, window = extracted
    return (
        "<HIVE-HINT>\n"
        f"{_hint_text(tokens, window)}\n"
        "</HIVE-HINT>"
    )
