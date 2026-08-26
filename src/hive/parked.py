"""Messages held back from a claude member that is mid-turn.

Claude Code renders an inbox message by *arrival timing*: one that lands while
the session is idle becomes a queued command, rendered exactly like something
the human typed; one that lands during a turn is wrapped in an interruption
banner. So hive parks a delivery aimed at a busy claude member and hands it
over the moment that member's own registry reports idle.

The sender is told ``ok`` at park time (its bus row is already written), so the
queue is durable: it lives in ``<workspace>/run/parked.jsonl`` and is rewritten
whenever an entry arrives or leaves — a team holds a handful of messages, which
is far below the size where a compaction scheme would earn its keep.

Ordering: entries are strictly FIFO per target and a row stays in the queue
until its hand-over finishes, so :meth:`ParkQueue.holds` keeps a fresh send
behind an in-flight one. That assumes a single flusher thread — the sidecar
runs exactly one.
"""

from __future__ import annotations

import json
import os
import sys
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any

PARKED_FILE_NAME = "parked.jsonl"
# A hold this old is handed over even to a still-busy member: a member that
# never goes idle must not swallow a delivery the sender was told was ok. The
# inbox `priority: later` keeps even a forced hand-over out of the running turn.
MAX_HOLD_SECONDS = 300.0


@dataclass
class ParkedMessage:
    team: str
    target: str
    msg_id: str
    envelope: str
    parked_at: float

    def to_row(self) -> dict[str, Any]:
        return {
            "team": self.team,
            "target": self.target,
            "msgId": self.msg_id,
            "envelope": self.envelope,
            "parkedAt": self.parked_at,
        }

    @classmethod
    def from_row(cls, row: Any) -> ParkedMessage | None:
        if not isinstance(row, dict):
            return None
        target = str(row.get("target") or "")
        msg_id = str(row.get("msgId") or "")
        envelope = str(row.get("envelope") or "")
        try:
            parked_at = float(row.get("parkedAt"))
        except (TypeError, ValueError):
            return None
        if not target or not msg_id or not envelope:
            return None
        return cls(
            team=str(row.get("team") or ""),
            target=target,
            msg_id=msg_id,
            envelope=envelope,
            parked_at=parked_at,
        )

    def held_for(self, now: float) -> float:
        return max(0.0, now - self.parked_at)

    def expired(self, now: float) -> bool:
        return self.held_for(now) >= MAX_HOLD_SECONDS


class ParkQueue:
    """File-backed FIFO of held messages, one per workspace."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._lock = threading.Lock()
        self._rows: list[ParkedMessage] = []
        self.skipped = self._load()

    def _load(self) -> int:
        try:
            text = self.path.read_text()
        except OSError:
            return 0
        skipped = 0
        for line in text.splitlines():
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except ValueError:
                skipped += 1
                continue
            message = ParkedMessage.from_row(row)
            if message is None:
                skipped += 1
                continue
            self._rows.append(message)
        return skipped

    def _persist(self) -> None:
        payload = "".join(
            json.dumps(row.to_row(), ensure_ascii=False) + "\n" for row in self._rows
        )
        tmp = self.path.with_name(f"{self.path.name}.{os.getpid()}.tmp")
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(payload)
            os.replace(tmp, self.path)
        except OSError as exc:
            # In-memory queue still flushes; only restart durability degrades.
            print(f"hive parked: persist to {self.path} failed: {exc}", file=sys.stderr)

    def park(self, message: ParkedMessage) -> None:
        with self._lock:
            self._rows.append(message)
            self._persist()

    def holds(self, team: str, target: str) -> bool:
        """True while a message for *target* is queued or being handed over."""
        with self._lock:
            return any(r.team == team and r.target == target for r in self._rows)

    def heads(self) -> list[ParkedMessage]:
        """The oldest held message per target, in arrival order."""
        with self._lock:
            seen: set[tuple[str, str]] = set()
            heads: list[ParkedMessage] = []
            for row in self._rows:
                key = (row.team, row.target)
                if key in seen:
                    continue
                seen.add(key)
                heads.append(row)
            return heads

    def release(self, message: ParkedMessage) -> None:
        with self._lock:
            self._rows = [row for row in self._rows if row is not message]
            self._persist()

    def pending(self) -> list[ParkedMessage]:
        with self._lock:
            return list(self._rows)
