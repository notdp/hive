"""Unit tests for draft_guard parsers.

Fixtures are real `tmux capture-pane` snapshots from Claude Code,
Codex, and Droid TUIs recorded during feature development.
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


def test_parse_codex_no_draft_block_returns_nothing():
    # Capture with no `› ` prompt line -> parser gives up.
    capture = """
• earlier turn

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == ""


def test_parse_codex_prompt_placeholder_text_is_returned_verbatim():
    # Codex renders `› Use /skills to list available skills` as gray
    # placeholder; the parser cannot distinguish placeholder from a user
    # draft that happens to start with that phrase. The gate relies on
    # cursor_x to avoid calling the parser in that empty state.
    capture = """
• earlier turn

› Use /skills to list available skills

  gpt-5.4 xhigh fast · ~/Developer/hive
"""
    assert draft_guard._parse_codex(_lines(capture)) == "Use /skills to list available skills"


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


def test_parse_droid_empty_input_returns_nothing():
    capture = """
 Auto (High) - allow all commands                                             Claude Opus 4.6 (Max)
╭─────────────────────────────────────────────────────────────────────────────────────╮
│ > Try "Create a PR with these changes"                                              │
╰─────────────────────────────────────────────────────────────────────────────────────╯
 ? for help                                                          MCP ✓ GHOSTTY ᗣ
 Claude Opus 4.6 · main                                             ~/Developer/hive
"""
    assert draft_guard._parse_droid(_lines(capture)) == ""


def test_parse_droid_multi_line_draft():
    capture = """
 Auto (High) - allow all commands                                             Claude Opus 4.6 (Max)
╭─────────────────────────────────────────────────────────────────────────────────────╮
│ > 送达发顺丰飒风                                                                    │
│   的卅风急浪大顺丰                                                                  │
│   11212123123                                                                       │
╰─────────────────────────────────────────────────────────────────────────────────────╯
 ? for help                                                          MCP ✓ GHOSTTY ᗣ
"""
    assert draft_guard._parse_droid(_lines(capture)) == (
        "送达发顺丰飒风\n的卅风急浪大顺丰\n11212123123"
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
        lambda _pane, lines=50: capture.lstrip("\n"),
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
        lambda _pane, lines=50: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "claude") is True


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
        lambda _pane, lines=50: capture.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "codex") is True
    assert draft_guard.parse_draft("%999", "codex") == "line 1\n\nline 2 after blank\n\nline 3"


def test_suspected_draft_droid_uses_capture(monkeypatch):
    capture_with_draft = """
╭──────╮
│ > hello                                                         │
╰──────╯
 status
"""
    capture_empty = """
╭──────╮
│ > Try "Create a PR with these changes"                           │
╰──────╯
 status
"""
    monkeypatch.setattr(draft_guard.tmux, "display_value", lambda *a, **kw: "30")
    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50: capture_with_draft.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "droid") is True

    monkeypatch.setattr(
        draft_guard.tmux,
        "capture_pane",
        lambda _pane, lines=50: capture_empty.lstrip("\n"),
    )
    assert draft_guard.suspected_draft("%999", "droid") is False


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
    draft_guard.clear_input("%42", "droid")
    assert len(sent) == 3
    assert sent[0][1] == ("C-u",) * 30
    assert sent[1][1] == ("C-u",) * 30
    assert sent[2][1] == ("C-u",) * 15
