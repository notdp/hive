"""Unit tests for the context_hint module."""

from __future__ import annotations

import pytest

from hive.context_hint import (
    CONTEXT_HINT_THRESHOLD,
    hint_block_for,
    maybe_attach_hint,
)


pytestmark = pytest.mark.unit


# Locked to the value the agent-facing skill contract advertises.
# Keep this assertion (rather than recomputing from the constant) so a
# silent code change cannot disagree with skills/hive/SKILL.md.
EXPECTED_THRESHOLD = 400_000


CLAUDE_SOURCE = "claude_assistant_usage"
CODEX_SOURCE = "codex_token_count_event"


def _runtime_with(
    tokens: int | None,
    *,
    window: int | None = None,
    source: str | None = CLAUDE_SOURCE,
) -> dict:
    member: dict = {}
    if tokens is not None:
        ctx: dict = {"tokens": tokens, "window": window}
        if source is not None:
            ctx["source"] = source
        member["context"] = ctx
    return {"members": {"orch": member}}


class TestThresholdContract:
    def test_threshold_matches_skill_contract(self):
        assert CONTEXT_HINT_THRESHOLD == EXPECTED_THRESHOLD


class TestMaybeAttachHint:
    def test_above_threshold_attaches_hint(self):
        payload: dict = {}
        tokens = EXPECTED_THRESHOLD + 25_000
        runtime = _runtime_with(tokens, window=1_000_000)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert "compactHint" in payload
        assert f"{tokens}/1000000" in payload["compactHint"]
        assert "hive compact" in payload["compactHint"]
        assert "hive inject" not in payload["compactHint"]

    def test_above_threshold_without_window_omits_window_part(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1, window=None)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload["compactHint"].startswith(
            f"context: {EXPECTED_THRESHOLD + 1} tokens."
        )
        assert "/1000000" not in payload["compactHint"]

    def test_at_threshold_does_not_attach(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert "compactHint" not in payload

    def test_below_threshold_does_not_attach(self):
        payload: dict = {}
        runtime = _runtime_with(100_000)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert "compactHint" not in payload

    def test_no_runtime_silently_skips(self):
        payload: dict = {}
        maybe_attach_hint(payload, self_name="orch", team_runtime=None)
        assert payload == {}

    def test_missing_member_silently_skips(self):
        payload: dict = {}
        runtime = {"members": {}}
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload == {}

    def test_member_without_context_silently_skips(self):
        payload: dict = {}
        runtime = {"members": {"orch": {"alive": True}}}
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload == {}

    def test_empty_self_name_skips(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1)
        maybe_attach_hint(payload, self_name="", team_runtime=runtime)
        assert payload == {}

    def test_does_not_overwrite_existing_compact_hint(self):
        payload: dict = {"compactHint": "preset"}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        # mutates: latest write wins
        assert payload["compactHint"] != "preset"

    def test_codex_source_above_threshold_does_not_attach(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 50_000, source=CODEX_SOURCE)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload == {}

    def test_missing_source_does_not_attach(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1, source=None)
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload == {}

    def test_unknown_source_does_not_attach(self):
        payload: dict = {}
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1, source="droid_unknown")
        maybe_attach_hint(payload, self_name="orch", team_runtime=runtime)
        assert payload == {}


class TestHintBlockFor:
    def test_above_threshold_returns_block(self):
        runtime = _runtime_with(EXPECTED_THRESHOLD + 1)
        block = hint_block_for(self_name="orch", team_runtime=runtime)
        assert block.startswith("<HIVE-HINT>")
        assert block.endswith("</HIVE-HINT>")
        assert "hive compact" in block
        assert "hive inject" not in block

    def test_below_threshold_returns_empty_string(self):
        runtime = _runtime_with(100_000)
        block = hint_block_for(self_name="orch", team_runtime=runtime)
        assert block == ""

    def test_no_runtime_returns_empty_string(self):
        block = hint_block_for(self_name="orch", team_runtime=None)
        assert block == ""

    def test_codex_source_above_threshold_returns_empty_string(self):
        runtime = _runtime_with(EXPECTED_THRESHOLD + 50_000, source=CODEX_SOURCE)
        block = hint_block_for(self_name="orch", team_runtime=runtime)
        assert block == ""
