"""The live mirror: transcript rows in, native-looking lines out."""
import json

import pytest

from hive import transcript_view as tv

pytestmark = pytest.mark.unit


def _row(kind, content, usage=None):
    msg = {"content": content}
    if usage:
        msg["usage"] = usage
    return json.dumps({"type": kind, "message": msg})


def test_assistant_text_renders_with_marker_and_markdown():
    r = tv._Renderer()
    out = r.render(_row("assistant", [{"type": "text", "text": "done: **all green**"}]))
    assert "⏺" in out and "\x1b[1mall green\x1b[0m" in out
    assert r.state == "idle"


def test_tool_use_prefers_the_human_readable_hint():
    r = tv._Renderer()
    out = r.render(_row("assistant", [{"type": "tool_use", "name": "Bash",
                                       "input": {"command": "ls", "description": "List files"}}]))
    assert "Bash" in out and "List files" in out and "ls" not in out.replace("List files", "")
    assert r.state == "working"


def test_hive_envelope_collapses_to_a_tagged_line():
    r = tv._Renderer()
    body = "<HIVE from=comb.dodo to=comb.rex msgId=a1>review the spec</HIVE>"
    out = r.render(_row("user", body))
    assert "✉" in out and "comb.dodo" in out and "review the spec" in out
    assert "<HIVE" not in out
    assert r.state == "working"


def test_user_turn_flips_working_and_final_text_flips_idle():
    r = tv._Renderer()
    r.render(_row("user", "hi"))
    assert r.state == "working"
    r.render(_row("assistant", [{"type": "text", "text": "hello"}]))
    assert r.state == "idle"


def test_output_tokens_accumulate_into_the_status_line():
    r = tv._Renderer()
    r.render(_row("assistant", [{"type": "text", "text": "a"}], usage={"output_tokens": 40}))
    r.render(_row("assistant", [{"type": "text", "text": "b"}], usage={"output_tokens": 2}))
    assert "42 tokens out" in r.status_line(0, "deadbeef-1234")


def test_non_message_rows_render_nothing():
    r = tv._Renderer()
    assert r.render(json.dumps({"type": "system"})) is None
    assert r.render("not json") is None
