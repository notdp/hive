//! Protect user drafts when injecting text into TUI input boxes.
//!
//! Normal Hive delivery never touches the composer: a claude member takes
//! the supervisor daemon's `op:"reply"` (the inbox socket is the fallback),
//! codex goes through the shared app-server daemon, grok through its
//! leader daemon. Two keystroke surfaces remain and are the callers here:
//! `agent::_submit_interactive_text` (`hive inject`, and `/compact`, which
//! is TUI vocabulary and has to be typed) uses the save/clear/restore
//! trio, and the claude bg keyboard lane's `_composer_has_draft` uses
//! `suspected_draft` to gate its kill-ring paste. On those paths a naive
//! `send-keys -l <msg>` + Enter would concatenate whatever the user was
//! typing with the injected text, so this saves the draft, clears the
//! input box, lets the caller inject + submit, then pastes the draft back
//! via bracketed paste so multi-line content does not trigger an accidental
//! submit.
//!
//! Profiles differ in prompt glyph and clear-keys cost:
//!
//! - claude: `❯ ` with NO-BREAK SPACE (U+00A0) separator; C-u × 30 drains
//!   the input box
//! - codex:  `› ` (U+203A + 0x20); C-u × 30

use std::time::{Duration, Instant};

use anyhow::Result;

const CODEX_PROMPT: &str = "\u{203a} "; // "› "
const CLAUDE_PROMPT: &str = "\u{276f}\u{a0}"; // "❯" + NBSP

const WAIT_INPUT_EMPTY_INTERVAL: Duration = Duration::from_millis(50);

struct ProfileConfig {
    name: &'static str,
    clear_repetitions: usize,
}

static PROFILES: [ProfileConfig; 2] = [
    ProfileConfig {
        name: "claude",
        clear_repetitions: 30,
    },
    ProfileConfig {
        name: "codex",
        clear_repetitions: 30,
    },
];

fn profile_config(profile_name: &str) -> Option<&'static ProfileConfig> {
    PROFILES.iter().find(|p| p.name == profile_name)
}

fn parser_for(profile_name: &str) -> Option<fn(&[String]) -> String> {
    match profile_name {
        "claude" => Some(parse_claude),
        "codex" => Some(parse_codex),
        _ => None,
    }
}

pub fn supported_profile(profile_name: &str) -> bool {
    profile_config(profile_name).is_some()
}

/// Gate: return true when the input box is non-empty.
///
/// Implemented by parsing the current capture. `cursor_x` was tried as
/// a cheap signal earlier but proved unreliable — the user can paste
/// content and move the cursor back to column 2 (empty baseline),
/// producing a false negative and silent draft pollution.
///
/// Parsing costs one `capture-pane` plus a profile-specific scan —
/// measured at a few ms, worth paying every inject.
pub fn suspected_draft(pane_id: &str, profile_name: &str) -> Result<bool> {
    if profile_config(profile_name).is_none() {
        return Ok(false);
    }
    let Some(parser) = parser_for(profile_name) else {
        return Ok(false);
    };
    Ok(!parser(&capture_lines(pane_id, profile_name)?).is_empty())
}

/// Parse the draft content from the TUI input box.
///
/// Returns "" if no draft or profile is unsupported. Does not catch
/// tmux errors — callers decide what to do on failure.
pub fn parse_draft(pane_id: &str, profile_name: &str) -> Result<String> {
    let Some(parser) = parser_for(profile_name) else {
        return Ok(String::new());
    };
    Ok(parser(&capture_lines(pane_id, profile_name)?))
}

/// Clear the TUI input box with a profile-specific C-u barrage.
pub fn clear_input(pane_id: &str, profile_name: &str) -> Result<()> {
    let reps = profile_config(profile_name)
        .map(|c| c.clear_repetitions)
        .unwrap_or(20);
    let keys: Vec<&str> = vec!["C-u"; reps];
    tmux_send_keys_batch(pane_id, &keys)
}

/// Poll until suspected_draft returns false. Return true on success.
///
/// Callers pick the timeout; the poll interval is a module constant because
/// no caller ever overrides it.
pub fn wait_input_empty(pane_id: &str, profile_name: &str, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !suspected_draft(pane_id, profile_name)? {
            return Ok(true);
        }
        std::thread::sleep(WAIT_INPUT_EMPTY_INTERVAL);
    }
    Ok(false)
}

fn capture_lines(pane_id: &str, profile_name: &str) -> Result<Vec<String>> {
    let height = tmux_display_value(pane_id, "#{pane_height}").unwrap_or_else(|| "80".to_string());
    let lines_arg: u32 = match height.parse::<u32>() {
        Ok(h) => h.max(30),
        Err(_) => 80,
    };
    let preserve_styles = matches!(profile_name, "claude" | "codex");
    let text = tmux_capture_pane(pane_id, lines_arg, preserve_styles)?;
    Ok(text.lines().map(str::to_string).collect())
}

// tmux boundary: thin wrappers the unit tests answer through `tests::mock_*`
// (canned display values and captures, recorded key batches) so no test
// reaches a tmux server.

fn tmux_display_value(target: &str, fmt: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(mocked) = tests::mock_display_value(target, fmt) {
        return mocked;
    }
    crate::tmux::display_value(target, fmt)
}

fn tmux_capture_pane(pane_id: &str, lines: u32, preserve_styles: bool) -> Result<String> {
    #[cfg(test)]
    if let Some(mocked) = tests::mock_capture_pane(pane_id, lines, preserve_styles) {
        return mocked;
    }
    crate::tmux::capture_pane(pane_id, lines, preserve_styles)
}

fn tmux_send_keys_batch(pane_id: &str, keys: &[&str]) -> Result<()> {
    #[cfg(test)]
    if let Some(mocked) = tests::mock_send_keys_batch(pane_id, keys) {
        return mocked;
    }
    crate::tmux::send_keys_batch(pane_id, keys)
}

#[derive(Clone, Copy)]
struct StyledChar {
    value: char,
    dim: bool,
    reverse: bool,
}

fn parse_claude(lines: &[String]) -> String {
    let seps: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let vis = visible_text(l);
            vis.starts_with('\u{2500}') && vis.chars().count() > 20
        })
        .map(|(i, _)| i)
        .collect();
    if seps.len() < 2 {
        return String::new();
    }
    let top = seps[seps.len() - 2] + 1;
    let bot = seps[seps.len() - 1];
    strip_styled_lines(&lines[top..bot], CLAUDE_PROMPT)
}

fn parse_codex(lines: &[String]) -> String {
    // Locate the last draft line (excluding status + trailing empty rows).
    let vis_empty = |idx: i64| visible_text(&lines[idx as usize]).trim().is_empty();
    let mut i = lines.len() as i64 - 1;
    while i >= 0 && vis_empty(i) {
        i -= 1;
    }
    while i >= 0 && !vis_empty(i) {
        i -= 1;
    }
    while i >= 0 && vis_empty(i) {
        i -= 1;
    }
    if i < 0 {
        return String::new();
    }
    let end = i as usize;
    // Walk upward for the `›` prompt row that opens the draft block.
    let mut start = None;
    for j in (0..=end).rev() {
        if visible_text(&lines[j]).starts_with(CODEX_PROMPT) {
            start = Some(j);
            break;
        }
    }
    let Some(start) = start else {
        return String::new();
    };
    strip_styled_lines(&lines[start..=end], CODEX_PROMPT)
}

fn strip_styled_lines(lines: &[String], first_prefix: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let mut cells = styled_chars(line);
        if idx == 0 {
            cells = drop_visible_prefix(cells, first_prefix);
            cells = drop_autocomplete_hint_cells(cells);
            // Match the old plain-text parser: if the prompt rendering leaves
            // one extra leading space before draft text, drop that boundary
            // space only on the first line.
            if cells.first().map(|c| c.value) == Some(' ') {
                cells.remove(0);
            }
        } else {
            cells = drop_visible_prefix(cells, "  ");
        }
        out.push(cells.iter().map(|c| c.value).collect());
    }
    out.join("\n")
}

fn drop_visible_prefix(cells: Vec<StyledChar>, prefix: &str) -> Vec<StyledChar> {
    let n = prefix.chars().count();
    let head: String = cells.iter().take(n).map(|c| c.value).collect();
    if head == prefix {
        cells.into_iter().skip(n).collect()
    } else {
        cells
    }
}

fn drop_autocomplete_hint_cells(mut cells: Vec<StyledChar>) -> Vec<StyledChar> {
    let Some(first_dim) = cells.iter().position(|c| c.dim) else {
        return cells;
    };
    let mut start = first_dim;
    while start > 0 && cells[start - 1].reverse {
        start -= 1;
    }
    cells.truncate(start);
    cells
}

fn visible_text(line: &str) -> String {
    styled_chars(line).iter().map(|c| c.value).collect()
}

fn styled_chars(line: &str) -> Vec<StyledChar> {
    let chars: Vec<char> = line.chars().collect();
    let mut cells: Vec<StyledChar> = Vec::new();
    let mut dim = false;
    let mut reverse = false;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '[' {
            let end = chars[i + 2..]
                .iter()
                .position(|&c| c == 'm')
                .map(|p| p + i + 2);
            if let Some(end) = end {
                let codes: String = chars[i + 2..end].iter().collect();
                let params = if codes.is_empty() {
                    vec![0]
                } else {
                    parse_sgr_codes(&codes)
                };
                (dim, reverse) = apply_sgr(&params, dim, reverse);
                i = end + 1;
                continue;
            }
        }
        cells.push(StyledChar {
            value: chars[i],
            dim,
            reverse,
        });
        i += 1;
    }
    cells
}

fn parse_sgr_codes(raw: &str) -> Vec<i64> {
    let mut codes: Vec<i64> = Vec::new();
    for part in raw.split(';') {
        if part.is_empty() {
            codes.push(0);
            continue;
        }
        if let Ok(n) = part.parse::<i64>() {
            codes.push(n);
        }
    }
    codes
}

fn apply_sgr(params: &[i64], mut dim: bool, mut reverse: bool) -> (bool, bool) {
    let mut i = 0;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => {
                dim = false;
                reverse = false;
            }
            2 => dim = true,
            7 => reverse = true,
            22 => dim = false,
            27 => reverse = false,
            38 | 48 => {
                if i + 1 < params.len() {
                    let mode = params[i + 1];
                    if mode == 2 {
                        i += 4;
                    } else if mode == 5 {
                        i += 2;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    (dim, reverse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct CaptureMock {
        text: String,
        seen_preserve_styles: Option<bool>,
    }

    thread_local! {
        static DISPLAY_VALUE: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
        static CAPTURE: RefCell<Option<CaptureMock>> = const { RefCell::new(None) };
        static SENT_KEYS: RefCell<Option<Vec<(String, Vec<String>)>>> = const { RefCell::new(None) };
    }

    pub(super) fn mock_display_value(_target: &str, _fmt: &str) -> Option<Option<String>> {
        DISPLAY_VALUE.with(|d| d.borrow().clone())
    }

    pub(super) fn mock_capture_pane(
        _pane_id: &str,
        _lines: u32,
        preserve_styles: bool,
    ) -> Option<Result<String>> {
        CAPTURE.with(|c| {
            c.borrow_mut().as_mut().map(|m| {
                m.seen_preserve_styles = Some(preserve_styles);
                Ok(m.text.clone())
            })
        })
    }

    pub(super) fn mock_send_keys_batch(pane_id: &str, keys: &[&str]) -> Option<Result<()>> {
        SENT_KEYS.with(|s| {
            s.borrow_mut().as_mut().map(|sent| {
                sent.push((
                    pane_id.to_string(),
                    keys.iter().map(|k| k.to_string()).collect(),
                ));
                Ok(())
            })
        })
    }

    fn set_display_value(value: &str) {
        DISPLAY_VALUE.with(|d| *d.borrow_mut() = Some(Some(value.to_string())));
    }

    fn set_capture(text: &str) {
        CAPTURE.with(|c| {
            *c.borrow_mut() = Some(CaptureMock {
                text: text.to_string(),
                seen_preserve_styles: None,
            })
        });
    }

    fn seen_preserve_styles() -> Option<bool> {
        CAPTURE.with(|c| c.borrow().as_ref().and_then(|m| m.seen_preserve_styles))
    }

    fn set_sent_keys_recorder() {
        SENT_KEYS.with(|s| *s.borrow_mut() = Some(Vec::new()));
    }

    fn sent_keys() -> Vec<(String, Vec<String>)> {
        SENT_KEYS.with(|s| s.borrow().clone().unwrap_or_default())
    }

    // Fixtures start with a newline so the screen text lines up in source.
    fn fixture_lines(text: &str) -> Vec<String> {
        text.trim_start_matches('\n')
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn fixture_capture(text: &str) -> String {
        text.trim_start_matches('\n').to_string()
    }

    #[test]
    fn test_parse_claude_empty_input_returns_nothing() {
        // Real Claude empty state: U+276F '❯' then U+00A0 NBSP (no body)
        let capture = concat!(
            "\n",
            " ▐▛███▜▌   Claude Code v2.1.111\n",
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}\n",
            "───────────────────────────────────────────────\n",
            "  status line\n",
        );
        assert_eq!(parse_claude(&fixture_lines(capture)), "");
    }

    #[test]
    fn test_parse_claude_dim_autocomplete_hint_is_not_treated_as_draft() {
        // Real Claude autocomplete state from `capture-pane -e`: hint text is
        // styled dim, sometimes with a reverse-video cursor cell at the front.
        let capture = concat!(
            "\n",
            " ▐▛███▜▌   Claude Code v2.1.111\n",
            "\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "\x1b[39m❯\u{a0}\x1b[7mp\x1b[0;2mush\x1b[0m\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "  status line\n",
        );
        assert_eq!(parse_claude(&fixture_lines(capture)), "");
    }

    #[test]
    fn test_parse_claude_user_draft_that_starts_with_try_is_preserved() {
        // Normal-style draft text must be preserved even when it begins with
        // text that used to be treated as a hardcoded placeholder.
        let capture = concat!(
            "\n",
            " ▐▛███▜▌   Claude Code v2.1.111\n",
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}Try this query\n",
            "  against the new index\n",
            "───────────────────────────────────────────────\n",
            "  status line\n",
        );
        assert_eq!(
            parse_claude(&fixture_lines(capture)),
            "Try this query\nagainst the new index"
        );
    }

    #[test]
    fn test_parse_claude_two_line_draft() {
        // Note: Claude uses U+276F '❯' followed by U+00A0 NO-BREAK SPACE as prompt
        let capture = concat!(
            "\n",
            " ▐▛███▜▌   Claude Code v2.1.111\n",
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}事发当时发生3\n",
            "  3记录2➕234\n",
            "───────────────────────────────────────────────\n",
            "  status line 1\n",
            "  status line 2\n",
        );
        assert_eq!(
            parse_claude(&fixture_lines(capture)),
            "事发当时发生3\n3记录2➕234"
        );
    }

    #[test]
    fn test_parse_claude_continuation_indentation_is_preserved() {
        let capture = concat!(
            "\n",
            " ▐▛███▜▌   Claude Code v2.1.111\n",
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}line 1\n",
            "    indented\n",
            "───────────────────────────────────────────────\n",
            "  status line\n",
        );
        assert_eq!(parse_claude(&fixture_lines(capture)), "line 1\n  indented");
    }

    #[test]
    fn test_parse_codex_no_draft_block_returns_nothing() {
        // Capture with no `› ` prompt line -> parser gives up.
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        assert_eq!(parse_codex(&fixture_lines(capture)), "");
    }

    #[test]
    fn test_parse_codex_single_line_real_draft_is_preserved() {
        // Normal-style single-line input must be returned as-is.
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "› hello team what's next\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        assert_eq!(
            parse_codex(&fixture_lines(capture)),
            "hello team what's next"
        );
    }

    #[test]
    fn test_parse_codex_dim_autocomplete_hint_is_not_treated_as_draft() {
        // Current Codex empty input from `capture-pane -e`: suggestion text is dim.
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "\x1b[1m›\x1b[0m\x1b[48;2;244;244;244m \x1b[2mExplain this codebase\x1b[0m\x1b[48;2;244;244;244m\n",
            "\n",
            "  gpt-5.5 xhigh · ~/Developer/hive\n",
        );
        assert_eq!(parse_codex(&fixture_lines(capture)), "");
    }

    #[test]
    fn test_parse_codex_user_draft_that_looks_like_old_placeholder_is_preserved() {
        // The old hardcoded placeholder text must not be special anymore when
        // it is rendered as normal draft text.
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "› Improve documentation in @filename\n",
            "  and also add a usage example\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        assert_eq!(
            parse_codex(&fixture_lines(capture)),
            "Improve documentation in @filename\nand also add a usage example"
        );
    }

    #[test]
    fn test_parse_codex_multi_line_draft_is_joined() {
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "› 阿斯顿发送发的卅\n",
            "  啊点手机费拉屎的积分啦水淀粉as\n",
            "  是氮磷钾肥打算减肥拉萨来到福建师大\n",
            "  11111\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        let result = parse_codex(&fixture_lines(capture));
        assert_eq!(
            result,
            "阿斯顿发送发的卅\n啊点手机费拉屎的积分啦水淀粉as\n是氮磷钾肥打算减肥拉萨来到福建师大\n11111"
        );
    }

    #[test]
    fn test_suspected_draft_unsupported_profile_returns_false() {
        assert!(!suspected_draft("%999", "unknown").unwrap());
    }

    #[test]
    fn test_suspected_draft_claude_empty_input_is_false() {
        let capture = concat!(
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}\n",
            "───────────────────────────────────────────────\n",
            "  status\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(!suspected_draft("%999", "claude").unwrap());
    }

    #[test]
    fn test_suspected_draft_claude_dim_hint_is_false() {
        let capture = concat!(
            "\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "\x1b[39m❯\u{a0}\x1b[2mPress up to edit queued messages\x1b[0m\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "  status\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(!suspected_draft("%999", "claude").unwrap());
    }

    #[test]
    fn test_suspected_draft_claude_with_text_is_true() {
        let capture = concat!(
            "\n",
            "───────────────────────────────────────────────\n",
            "❯\u{a0}hello world\n",
            "───────────────────────────────────────────────\n",
            "  status\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(suspected_draft("%999", "claude").unwrap());
    }

    #[test]
    fn test_suspected_draft_claude_uses_styled_capture_for_autocomplete() {
        let capture = concat!(
            "\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "\x1b[39m❯\u{a0}\x1b[7mp\x1b[0;2mush\x1b[0m\n",
            "\x1b[38;5;246m───────────────────────────────────────────────\x1b[39m\n",
            "  status\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(!suspected_draft("%999", "claude").unwrap());
        assert_eq!(seen_preserve_styles(), Some(true));
    }

    #[test]
    fn test_suspected_draft_codex_multi_paragraph_is_true() {
        // Earlier bug: blank line between paragraphs terminated the scan
        // early and the parser returned only the last paragraph. A paragraph
        // above the blank line must still count as draft.
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "› line 1\n",
            "\n",
            "  line 2 after blank\n",
            "\n",
            "  line 3\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(suspected_draft("%999", "codex").unwrap());
        assert_eq!(
            parse_draft("%999", "codex").unwrap(),
            "line 1\n\nline 2 after blank\n\nline 3"
        );
    }

    #[test]
    fn test_suspected_draft_codex_uses_styled_capture_for_autocomplete() {
        let capture = concat!(
            "\n",
            "• earlier turn\n",
            "\n",
            "\x1b[1m›\x1b[0m\x1b[48;2;244;244;244m \x1b[2mExplain this codebase\x1b[0m\x1b[48;2;244;244;244m\n",
            "\n",
            "  gpt-5.5 xhigh · ~/Developer/hive\n",
        );
        set_display_value("30");
        set_capture(&fixture_capture(capture));
        assert!(!suspected_draft("%999", "codex").unwrap());
        assert_eq!(seen_preserve_styles(), Some(true));
    }

    #[test]
    fn test_parse_codex_first_line_drops_extra_space_from_paste() {
        // Codex sometimes renders `›  <text>` (two spaces) when the user
        // pasted with a leading blank; parser should not leak it.
        let capture = concat!(
            "\n",
            "• earlier\n",
            "\n",
            "›  hello\n",
            "\n",
            "  gpt-5.4 xhigh fast · ~/Developer/hive\n",
        );
        assert_eq!(parse_codex(&fixture_lines(capture)), "hello");
    }

    #[test]
    fn test_clear_input_sends_profile_specific_batch() {
        set_sent_keys_recorder();
        clear_input("%42", "claude").unwrap();
        clear_input("%42", "codex").unwrap();
        let sent = sent_keys();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "%42");
        assert_eq!(sent[0].1, vec!["C-u"; 30]);
        assert_eq!(sent[1].0, "%42");
        assert_eq!(sent[1].1, vec!["C-u"; 30]);
    }
}
