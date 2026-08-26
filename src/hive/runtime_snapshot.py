"""Runtime snapshot primitives used by the sidecar.

Current scope is sessionId-only. This keeps the owner boundary narrow: the
sidecar maintains current-session identity, and snapshot-only consumers such
as cvim read that identity without launching their own live probes.

Stale snapshots may retain the last observed value for diagnostics, but
consumers must treat ``RuntimeField.is_fresh()`` / ``_sessionIdFresh`` as the
authority on whether that value can be used.

Phase 3+ candidates, intentionally postponed until there is measured demand:
- cheap pane facts such as profile/model when repeated query cost matters
- transcript-derived fields such as inputState/turnPhase/gate
- transcriptPath caching tied atomically to the same session generation

Those fields need explicit invalidation and same-generation semantics before
they move into this store. Until then, sidecar runtime queries compute them
on demand.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class RuntimeField:
    value: Any
    source: str
    observed_at: float
    generation: int
    freshness_s: float | None = None

    def is_fresh(self, *, now: float | None = None) -> bool:
        if self.freshness_s is None:
            return True
        return ((time.monotonic() if now is None else now) - self.observed_at) <= self.freshness_s


@dataclass(frozen=True)
class RuntimeSnapshot:
    pane_id: str
    generation: int
    sessionId: RuntimeField

    def to_runtime_fields(self, *, now: float | None = None) -> dict[str, Any]:
        session = self.sessionId
        payload = {
            "sessionId": session.value,
            "_sessionIdSource": session.source,
            "_runtimeGeneration": self.generation,
            "_sessionIdObservedAt": session.observed_at,
            "_sessionIdFresh": session.is_fresh(now=now),
        }
        if session.freshness_s is not None:
            payload["_sessionIdFreshnessS"] = session.freshness_s
        return payload


@dataclass
class RuntimeSnapshotStore:
    snapshots: dict[str, RuntimeSnapshot] = field(default_factory=dict)
    generation: int = 0

    def get(self, pane_id: str) -> RuntimeSnapshot | None:
        return self.snapshots.get(pane_id)

    def update_session_id(
        self,
        pane_id: str,
        session_id: str,
        *,
        source: str,
        observed_at: float | None = None,
        freshness_s: float | None = None,
    ) -> RuntimeSnapshot:
        self.generation += 1
        generation = self.generation
        field = RuntimeField(
            value=session_id,
            source=source,
            observed_at=time.monotonic() if observed_at is None else observed_at,
            generation=generation,
            freshness_s=freshness_s,
        )
        snapshot = RuntimeSnapshot(
            pane_id=pane_id,
            generation=generation,
            sessionId=field,
        )
        self.snapshots[pane_id] = snapshot
        return snapshot

    def clear(self) -> None:
        self.snapshots.clear()
        self.generation = 0
