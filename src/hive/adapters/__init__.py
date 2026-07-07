"""Session adapter registry.

Adapters normalize the per-CLI session on-disk format (droid/claude/codex) to
a single :class:`~hive.adapters.base.SessionAdapter` protocol. Callers should
route through :func:`get` by CLI name instead of branching on ``if name == ...``.
"""

from __future__ import annotations

from .base import Message, MessagePart, SessionAdapter, SessionMeta
from .claude import ClaudeAdapter
from .codex import CodexAdapter
from .droid import DroidAdapter

REGISTRY: dict[str, SessionAdapter] = {
    DroidAdapter.name: DroidAdapter(),
    ClaudeAdapter.name: ClaudeAdapter(),
    CodexAdapter.name: CodexAdapter(),
}


def get(name: str) -> SessionAdapter | None:
    return REGISTRY.get(name)


def available() -> list[str]:
    return list(REGISTRY.keys())


__all__ = [
    "Message",
    "MessagePart",
    "REGISTRY",
    "SessionAdapter",
    "SessionMeta",
    "available",
    "get",
]
