"""Unit tests for draft_guard parsers.

Fixtures are real `tmux capture-pane` snapshots from Claude Code
and Codex TUIs recorded during feature development.
"""

from hive import draft_guard


def _lines(text: str) -> list[str]:
    # Leading newline lets us keep triple-quoted fixtures tidy.
    return text.lstrip("\n").splitlines()


def test_parse_claude_empty_input_returns_nothing():
    # Real Claude empty state: U+276F '❯' then U+00A0 NBSP (no body)
    capture = f"""
 ▐▛███▜▌   Claude Code v2.1.111

───────────────────────────────────────────────
❯\xa0
───────────────────────────────────────────────
  status line
"""
    assert draft_guard._parse_claude(_lines(capture)) == ""


def test_parse_claude_dim_autocomplete_hint_is_not_treated_as_draft():
    # Real Claude autocomplete state from `capture-pane -e`: hint text is
    # styled dim, sometimes with a reverse-video cursor cell at the front.
    capture = f"""
 ▐▛███▜▌   Claude Code v2.1.111

\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
\x1b[39m❯\xa0\x1b[7mp\x1b[0;2mush\x1b[0m
\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
  status line
"""
    assert draft_guard._parse_claude(_lines(capture)) == ""


def test_parse_claude_user_draft_that_starts_with_try_is_preserved():
    # Normal-style draft text must be preserved even when it begins with text
    # that used to be treated as a hardcoded placeholder.
    capture = f"""
 ▐▛███▜▌   Claude Code v2.1.111

───────────────────────────────────────────────
❯\xa0Try this query
  against the new index
───────────────────────────────────────────────
  status line
"""
    assert draft_guard._parse_claude(_lines(capture)) == "Try this query\nagainst the new index"


def test_parse_claude_two_line_draft():
    # Note: Claude uses U+276F '❯' followed by U+00A0 NO-BREAK SPACE as prompt
    capture = """
 ▐▛███▜▌   Claude Code v2.1.111

───────────────────────────────────────────────
❯\xa0事发当时发生3
  3记录2➕234
───────────────────────────────────────────────
  status line 1
  status line 2
"""
    assert draft_guard._parse_claude(_lines(capture)) == "事发当时发生3\n3记录2➕234"


def test_parse_claude_continuation_indentation_is_preserved():
    capture = """
 ▐▛███▜▌   Claude Code v2.1.111

───────────────────────────────────────────────
❯\xa0line 1
    indented
───────────────────────────────────────────────
  status line
"""
    assert draft_guard._parse_claude(_lines(capture)) == "line 1\n  indented"


def test_parse_codex_no_draft_block_returns_nothing():
    # Capture with no `› ` prompt line -> parser gives up.
    capture = """
• earlier turn

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == ""


def test_parse_codex_single_line_real_draft_is_preserved():
    # Normal-style single-line input must be returned as-is.
    capture = """
• earlier turn

› hello team what's next

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == "hello team what's next"


def test_parse_codex_dim_autocomplete_hint_is_not_treated_as_draft():
    # Current Codex empty input from `capture-pane -e`: suggestion text is dim.
    capture = """
• earlier turn

\x1b[1m›\x1b[0m\x1b[48;2;244;244;244m \x1b[2mExplain this codebase\x1b[0m\x1b[48;2;244;244;244m

  gpt-5.5 xhigh · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == ""


def test_parse_codex_user_draft_that_looks_like_old_placeholder_is_preserved():
    # The old hardcoded placeholder text must not be special anymore when it is
    # rendered as normal draft text.
    capture = """
• earlier turn

› Improve documentation in @filename
  and also add a usage example

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == (
        "Improve documentation in @filename\nand also add a usage example"
    )


def test_parse_codex_multi_line_draft_is_joined():
    capture = """
• earlier turn

› 阿斯顿发送发的卅
  啊点手机费拉屎的积分啦水淀粉as
  是氮磷钾肥打算减肥拉萨来到福建师大
  11111

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    result = draft_guard._parse_codex(_lines(capture))
    assert result == (
        "阿斯顿发送发的卅\n"
        "啊点手机费拉屎的积分啦水淀粉as\n"
        "是氮磷钾肥打算减肥拉萨来到福建师大\n"
        "11111"
    )


def test_suspected_draft_unsupported_profile_returns_false(monkeypatch):
    assert draft_guard.suspected_draft("%999", "unknown") is False


def test_suspected_draft_claude_empty_input_is_false(monkeypatch):
    capture = """
───────────────────────────────────────────────
❯\xa0
───────────────────────────────────────────────
  status
"""
    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50, preserve_styles=False: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "claude") is False


def test_suspected_draft_claude_dim_hint_is_false(monkeypatch):
    capture = """
\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
\x1b[39m❯\xa0\x1b[2mPress up to edit queued messages\x1b[0m
\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
  status
"""
    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50, preserve_styles=False: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "claude") is False


def test_suspected_draft_claude_with_text_is_true(monkeypatch):
    capture = """
───────────────────────────────────────────────
❯\xa0hello world
───────────────────────────────────────────────
  status
"""
    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50, preserve_styles=False: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "claude") is True


def test_suspected_draft_claude_uses_styled_capture_for_autocomplete(monkeypatch):
    capture = """
\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
\x1b[39m❯\xa0\x1b[7mp\x1b[0;2mush\x1b[0m
\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m
  status
"""
    seen: dict[str, bool] = {}

    def fake_capture(_pane, lines=50, preserve_styles=False):
        seen["preserve_styles"] = preserve_styles
        return capture.lstrip("\n")

    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(draft_guard.tmux, "capture_pane", fake_capture)

    assert draft_guard.suspected_draft("%999", "claude") is False
    assert seen["preserve_styles"] is True


def test_suspected_draft_codex_multi_paragraph_is_true(monkeypatch):
    # Earlier bug: blank line between paragraphs terminated the scan
    # early and the parser returned only the last paragraph. A paragraph
    # above the blank line must still count as draft.
    capture = """
• earlier turn

› line 1

  line 2 after blank

  line 3

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50, preserve_styles=False: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "codex") is True
    assert draft_guard.parse_draft("%999", "codex") == "line 1\n\nline 2 after blank\n\nline 3"


def test_suspected_draft_codex_uses_styled_capture_for_autocomplete(monkeypatch):
    capture = """
• earlier turn

\x1b[1m›\x1b[0m\x1b[48;2;244;244;244m \x1b[2mExplain this codebase\x1b[0m\x1b[48;2;244;244;244m

  gpt-5.5 xhigh · ~/Developer/hive
"""
    seen: dict[str, bool] = {}

    def fake_capture(_pane, lines=50, preserve_styles=False):
        seen["preserve_styles"] = preserve_styles
        return capture.lstrip("\n")

    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(draft_guard.tmux, "capture_pane", fake_capture)

    assert draft_guard.suspected_draft("%999", "codex") is False
    assert seen["preserve_styles"] is True


def test_parse_codex_first_line_drops_extra_space_from_paste():
    # Codex sometimes renders `›  <text>` (two spaces) when the user
    # pasted with a leading blank; parser should not leak it.
    capture = """
• earlier

›  hello

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == "hello"


def test_clear_input_sends_profile_specific_batch(monkeypatch):
    sent: list[tuple[str, tuple[str, ...]]] = []

    def fake_batch(pane, *keys):
        sent.append((pane, keys))

    monkeypatch.setattr(draft_guard.tmux, "send_keys_batch", fake_batch)
    draft_guard.clear_input("%42", "claude")
    draft_guard.clear_input("%42", "codex")
    assert len(sent) == 2
    assert sent[0][1] == ("C-u",) * 30
    assert sent[1][1] == ("C-u",) * 30
