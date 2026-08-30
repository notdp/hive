//! Ratatui TUI for `hive view` — the Grok Build look over the
//! [`crate::transcript_view`] display-block model, in the resolved
//! [`crate::view_theme`] (grokday light / groknight dark).
//!
//! Chrome: full-screen bg fill, 2-col outer inset, top status line
//! (branch / worktree / `~`-abbreviated cwd, right-aligned token counter),
//! scrollback (full-width user bands, `◈`/`◆` tool lines, grok-markdown
//! assistant text with right-aligned timestamps, muted `Worked for …`), and a
//! single muted bottom hint row. Read-only: keys and the mouse wheel only
//! quit and scroll, with grok's pager bindings and follow semantics.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::transcript_view::{
    grok_md, hive_envelope, AssistantBlock, DisplayBlock, ThinkingBlock, TranscriptParser,
    UserBlock,
};
use crate::view_theme::ViewTheme;

/// jsonl poll cadence.
const POLL_MS: u64 = 250;
/// Backlog rows rendered from the tail of the transcript.
const TAIL_EVENTS: usize = 200;
/// Claude context window used as the token-counter denominator; sessions
/// whose usage exceeds it are on the large (1M) context window.
const CONTEXT_TOTAL: i64 = 200_000;
const CONTEXT_TOTAL_LARGE: i64 = 1_000_000;
/// Scrollback row chrome: accent col (1) + left pad (2) + right pad (2).
const ROW_CHROME: usize = 5;
/// Columns reserved right of the content area for the timestamp overlay.
const TS_RESERVE: usize = 10;
/// User band folds past this many visual lines (grok COLLAPSED_MAX_LINES).
const BAND_MAX_LINES: usize = 3;
/// One mouse-wheel tick scrolls this many lines (grok mouse.rs
/// DEFAULT_WHEEL_LINES_PER_TICK).
const WHEEL_LINES: usize = 3;

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

// ---------------------------------------------------------------------------
// Chrome state scraped from raw transcript rows
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Chrome {
    branch: Option<String>,
    cwd: Option<String>,
    /// Latest full-context usage (input + cache + output) from an assistant row.
    context_used: i64,
}

impl Chrome {
    fn update(&mut self, row: &Value) {
        if let Some(b) = row.get("gitBranch").and_then(Value::as_str) {
            if !b.is_empty() {
                self.branch = Some(b.to_string());
            }
        }
        if let Some(c) = row.get("cwd").and_then(Value::as_str) {
            if !c.is_empty() {
                self.cwd = Some(c.to_string());
            }
        }
        if let Some(usage) = row.get("message").and_then(|m| m.get("usage")) {
            let n = |k: &str| usage.get(k).and_then(Value::as_i64).unwrap_or(0);
            let used = n("input_tokens")
                + n("cache_creation_input_tokens")
                + n("cache_read_input_tokens")
                + n("output_tokens");
            if used > 0 {
                self.context_used = used;
            }
        }
    }
}

/// `$HOME`-prefixed paths abbreviate to `~` on a component boundary.
fn abbreviate_path(path: &str) -> String {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => return path.to_string(),
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// grok context_bar.rs::fmt_tokens (≤ 4 chars; 10K+ truncates, not rounds).
fn fmt_tokens(n: i64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", n / 1_000_000)
    }
}

// ---------------------------------------------------------------------------
// Column-accurate span utilities (unicode-width; CJK never straddles)
// ---------------------------------------------------------------------------

type Cell = (char, Style);

fn line_cells(line: &Line) -> Vec<Cell> {
    let mut cells = Vec::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            if ch == '\t' {
                cells.push((' ', span.style));
                cells.push((' ', span.style));
            } else if !ch.is_control() {
                cells.push((ch, span.style));
            }
        }
    }
    cells
}

fn cell_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|&(ch, _)| cell_width(ch)).sum()
}

/// Rebuild a line, merging adjacent same-style cells into single spans.
fn cells_to_line(cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut style: Option<Style> = None;
    for &(ch, st) in cells {
        if style != Some(st) {
            if let Some(prev) = style.take() {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            style = Some(st);
        }
        buf.push(ch);
    }
    if let Some(prev) = style {
        spans.push(Span::styled(buf, prev));
    }
    Line::from(spans)
}

/// Word-aware wrap of one styled line into display-column budget `width`.
fn wrap_cells(cells: Vec<Cell>, width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(2);
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut curw = 0usize;
    for (ch, st) in cells {
        let w = cell_width(ch);
        if curw + w > width && curw > 0 {
            if ch == ' ' {
                // The line ends exactly at this break point: emit as-is and
                // swallow the space instead of re-breaking at an earlier one.
                while cur.last().is_some_and(|&(c, _)| c == ' ') {
                    cur.pop();
                }
                lines.push(std::mem::take(&mut cur));
                curw = 0;
                continue;
            }
            if let Some(sp) = cur.iter().rposition(|&(c, _)| c == ' ') {
                let rest = cur.split_off(sp + 1);
                while cur.last().is_some_and(|&(c, _)| c == ' ') {
                    cur.pop();
                }
                lines.push(std::mem::replace(&mut cur, rest));
            } else {
                lines.push(std::mem::take(&mut cur));
            }
            curw = cells_width(&cur);
            if ch == ' ' && curw == 0 {
                continue;
            }
        }
        cur.push((ch, st));
        curw += w;
    }
    lines.push(cur);
    lines
}

fn wrap_line(line: &Line, width: usize) -> Vec<Line<'static>> {
    wrap_cells(line_cells(line), width)
        .iter()
        .map(|cells| cells_to_line(cells))
        .collect()
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let style = Style::default();
    let cells: Vec<Cell> = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .map(|c| (if c == '\t' { ' ' } else { c }, style))
        .collect();
    wrap_cells(cells, width)
        .iter()
        .map(|cells| cells.iter().map(|&(c, _)| c).collect())
        .collect()
}

/// Clip to `width` columns; on overflow a `…` inherits the last kept style.
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let line = Line::from(spans);
    let cells = line_cells(&line);
    if cells_width(&cells) <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let budget = width - 1;
    let mut acc = 0usize;
    let mut keep = 0usize;
    for (i, &(ch, _)) in cells.iter().enumerate() {
        let w = cell_width(ch);
        if acc + w > budget {
            break;
        }
        acc += w;
        keep = i + 1;
    }
    let kept = &cells[..keep];
    let style = kept.last().map(|&(_, st)| st).unwrap_or_default();
    let mut line = cells_to_line(kept);
    line.spans.push(Span::styled("…", style));
    line
}

fn clip_plain(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = cell_width(ch);
        if w + cw > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

// ---------------------------------------------------------------------------
// Block renderers
// ---------------------------------------------------------------------------

/// Band row: 3-col margin + spans + fill, everything on `bg_light`.
fn band_line(t: &ViewTheme, spans: Vec<Span<'static>>, inner_w: usize) -> Line<'static> {
    let band = Style::default().bg(t.bg_light);
    let mut all = vec![Span::styled("   ", band)];
    all.extend(spans);
    let line = Line::from(all);
    let used = cells_width(&line_cells(&line));
    let mut all = line.spans;
    if inner_w > used {
        all.push(Span::styled(" ".repeat(inner_w - used), band));
    }
    Line::from(all)
}

/// The raw `<HIVE …>` opening tag, for the envelope head line.
fn envelope_head(text: &str) -> Option<&str> {
    let start = text.find("<HIVE")?;
    let end = text[start..].find('>')? + start;
    Some(&text[start..=end])
}

fn push_user_block(t: &ViewTheme, out: &mut Vec<Line<'static>>, u: &UserBlock, inner_w: usize) {
    let band = Style::default().bg(t.bg_light);
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let bw = cw.saturating_sub(TS_RESERVE + 2).max(8);
    let mut srcs: Vec<String> = Vec::new();
    if u.is_hive_envelope {
        if let Some((_, body)) = hive_envelope(&u.text) {
            if let Some(head) = envelope_head(&u.text) {
                srcs.push(head.to_string());
            }
            srcs.extend(body.lines().map(str::to_string));
        }
    }
    if srcs.is_empty() {
        srcs.extend(u.text.lines().map(str::to_string));
    }
    let mut vis: Vec<String> = Vec::new();
    for src in &srcs {
        vis.extend(wrap_plain(src, bw));
    }
    while vis.last().is_some_and(|l| l.is_empty()) && vis.len() > 1 {
        vis.pop();
    }
    if vis.len() > BAND_MAX_LINES {
        vis.truncate(BAND_MAX_LINES);
        let last = vis.pop().unwrap_or_default();
        vis.push(format!("{} …", clip_plain(&last, bw.saturating_sub(2))));
    }
    out.push(band_line(t, Vec::new(), inner_w)); // vpad top
    for (i, text) in vis.iter().enumerate() {
        let prefix = if i == 0 { "❯ " } else { "  " };
        let mut spans = vec![
            Span::styled(prefix.to_string(), fg(t.accent_user).bg(t.bg_light)),
            Span::styled(text.clone(), fg(t.text_primary).bg(t.bg_light)),
        ];
        if i == 0 {
            if let Some(ts) = u.timestamp.as_ref() {
                let used = 2 + UnicodeWidthStr::width(text.as_str());
                let ts_text = format!("  {}", ts.clock);
                let ts_w = UnicodeWidthStr::width(ts_text.as_str());
                if cw > used + ts_w {
                    spans.push(Span::styled(" ".repeat(cw - used - ts_w), band));
                    spans.push(Span::styled(ts_text, fg(t.gray).bg(t.bg_light)));
                }
            }
        }
        out.push(band_line(t, spans, inner_w));
    }
    out.push(band_line(t, Vec::new(), inner_w)); // vpad bottom
}

fn push_assistant_block(
    t: &ViewTheme,
    out: &mut Vec<Line<'static>>,
    a: &AssistantBlock,
    inner_w: usize,
) {
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let aw = cw.saturating_sub(TS_RESERVE).max(10);
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in grok_md::render_ratatui(&a.markdown, t) {
        wrapped.extend(wrap_line(&line, aw));
    }
    while wrapped.last().is_some_and(|l| line_cells(l).is_empty()) {
        wrapped.pop();
    }
    for (i, line) in wrapped.into_iter().enumerate() {
        let mut spans = vec![Span::raw("   ")];
        let used = cells_width(&line_cells(&line));
        spans.extend(line.spans);
        if i == 0 {
            if let Some(ts) = a.timestamp.as_ref() {
                let ts_text = format!("  {}", ts.clock);
                let ts_w = UnicodeWidthStr::width(ts_text.as_str());
                if cw > used + ts_w {
                    spans.push(Span::raw(" ".repeat(cw - used - ts_w)));
                    spans.push(Span::styled(ts_text, fg(t.gray)));
                }
            }
        }
        out.push(Line::from(spans));
    }
}

fn thinking_spans(t: &ViewTheme, tb: &ThinkingBlock) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled("◆ ", fg(t.gray)),
        Span::styled("Thought", fg(t.gray).add_modifier(Modifier::BOLD)),
    ];
    if let Some(secs) = tb.duration_secs {
        spans.push(Span::styled(
            format!(
                " for {}",
                crate::transcript_view::format_thinking_duration(secs)
            ),
            fg(t.gray),
        ));
    }
    spans
}

fn push_block(
    t: &ViewTheme,
    out: &mut Vec<Line<'static>>,
    block: &DisplayBlock,
    inner_w: usize,
    last_dense: &mut bool,
) {
    let dense = matches!(
        block,
        DisplayBlock::ToolGroup(_)
            | DisplayBlock::Run(_)
            | DisplayBlock::Tool(_)
            | DisplayBlock::Thinking(_)
    );
    if !out.is_empty() && !(dense && *last_dense) {
        out.push(Line::default());
    }
    *last_dense = dense;
    let margin = Span::raw("   ");
    match block {
        DisplayBlock::User(u) => push_user_block(t, out, u, inner_w),
        DisplayBlock::Assistant(a) => push_assistant_block(t, out, a, inner_w),
        DisplayBlock::ToolGroup(g) => {
            let failed = g.failed();
            let bullet = if failed > 0 { t.accent_error } else { t.gray };
            let mut spans = vec![
                margin,
                Span::styled("◈ ", fg(bullet)),
                Span::styled(g.label(), fg(t.gray_bright).add_modifier(Modifier::BOLD)),
            ];
            if failed > 0 {
                spans.push(Span::styled(
                    format!(" · {failed} failed"),
                    fg(t.accent_error),
                ));
            }
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Run(r) => {
            let err = r.result.as_ref().is_some_and(|res| res.is_error);
            let bullet = if err { t.accent_error } else { t.gray };
            let spans = vec![
                margin,
                Span::styled("◆ ", fg(bullet)),
                Span::styled("Run ", fg(t.gray).add_modifier(Modifier::BOLD)),
                Span::styled(r.description.clone(), fg(t.gray)),
            ];
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Tool(tool) => {
            let err = tool.result.as_ref().is_some_and(|res| res.is_error);
            let bullet = if err { t.accent_error } else { t.gray };
            let mut spans = vec![
                margin,
                Span::styled("◆ ", fg(bullet)),
                Span::styled(tool.name.clone(), fg(t.gray).add_modifier(Modifier::BOLD)),
            ];
            if !tool.hint.is_empty() {
                spans.push(Span::styled(format!("  {}", tool.hint), fg(t.gray)));
            }
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Thinking(tb) => {
            let mut spans = vec![margin];
            spans.extend(thinking_spans(t, tb));
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::WorkedFor(w) => {
            let spans = vec![margin, Span::styled(w.label(), fg(t.gray))];
            out.push(clip_spans(spans, inner_w));
        }
    }
}

// ---------------------------------------------------------------------------
// Scroll/follow state (grok scrollback/state/nav.rs semantics)
// ---------------------------------------------------------------------------

struct Scroll {
    offset: usize,
    max: usize,
    follow: bool,
}

impl Scroll {
    fn new() -> Self {
        Scroll {
            offset: 0,
            max: 0,
            follow: true,
        }
    }

    /// The renderer feeds the fresh max offset; follow pins to the bottom.
    fn sync(&mut self, max: usize) {
        self.max = max;
        if self.follow || self.offset > max {
            self.offset = max;
        }
    }

    /// Any upward scroll leaves follow mode (nav.rs scroll_up).
    fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.offset = self.offset.saturating_sub(rows);
    }

    /// Downward scroll clamps to the bottom. Follow re-engages only on
    /// overscroll — a down gesture that was already pinned at the bottom and
    /// moved zero rows (nav.rs scroll_down + follow_by_overscroll; grok's
    /// j-on-last-entry rule collapses to the same gesture here). A scroll
    /// that merely lands at the bottom does NOT re-engage.
    fn scroll_down(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let before = self.offset;
        self.offset = (self.offset + rows).min(self.max);
        if self.offset == before && before == self.max {
            self.follow = true;
        }
    }

    /// `g` (nav.rs goto_top): jump to the top, follow off.
    fn goto_top(&mut self) {
        self.follow = false;
        self.offset = 0;
    }

    /// `G` (nav.rs goto_bottom): jump to the bottom AND re-engage follow.
    fn goto_bottom(&mut self) {
        self.follow = true;
        self.offset = self.max;
    }
}

/// Page scroll = viewport − 2 overlap rows, min 1 (nav.rs page_scroll_rows;
/// hive view has no sticky header).
fn page_rows(viewport_h: usize) -> usize {
    viewport_h.saturating_sub(2).max(1)
}

/// Half page = viewport / 2, min 1 (nav.rs half_page_up/down).
fn half_page_rows(viewport_h: usize) -> usize {
    (viewport_h / 2).max(1)
}

/// grok's scrollback-focused pager bindings (pager defaults.rs), reduced to
/// the read-only surface: j/k/arrows line scroll, Ctrl+J/K line scroll,
/// Ctrl+D/U half page, PageUp/Down page, g/G top/bottom, wheel ±3. `q` quits
/// directly (grok's read-only dashboard-overlay semantics) plus Ctrl+Q and
/// Ctrl+C. Returns true when the key quits the viewer.
fn handle_key(scroll: &mut Scroll, viewport_h: usize, code: KeyCode, mods: KeyModifiers) -> bool {
    if mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('c') | KeyCode::Char('q') => return true,
            KeyCode::Char('j') => scroll.scroll_down(1),
            KeyCode::Char('k') => scroll.scroll_up(1),
            KeyCode::Char('d') => scroll.scroll_down(half_page_rows(viewport_h)),
            KeyCode::Char('u') => scroll.scroll_up(half_page_rows(viewport_h)),
            _ => {}
        }
        return false;
    }
    match code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('j') | KeyCode::Down => scroll.scroll_down(1),
        KeyCode::Char('k') | KeyCode::Up => scroll.scroll_up(1),
        KeyCode::Char('g') => scroll.goto_top(),
        KeyCode::Char('G') => scroll.goto_bottom(),
        KeyCode::PageDown => scroll.scroll_down(page_rows(viewport_h)),
        KeyCode::PageUp => scroll.scroll_up(page_rows(viewport_h)),
        _ => {}
    }
    false
}

/// Wheel tick = 3 lines (grok mouse.rs), same follow rules as key scrolls.
fn handle_mouse(scroll: &mut Scroll, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollDown => scroll.scroll_down(WHEEL_LINES),
        MouseEventKind::ScrollUp => scroll.scroll_up(WHEEL_LINES),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// App state & frame chrome
// ---------------------------------------------------------------------------

struct App {
    theme: &'static ViewTheme,
    parser: TranscriptParser,
    chrome: Chrome,
    finalized: Vec<DisplayBlock>,
    cache: Vec<Line<'static>>,
    cached_blocks: usize,
    cache_width: usize,
    cache_last_dense: bool,
    scroll: Scroll,
    viewport_h: usize,
}

impl App {
    fn new(theme: &'static ViewTheme) -> Self {
        App {
            theme,
            parser: TranscriptParser::new(),
            chrome: Chrome::default(),
            finalized: Vec::new(),
            cache: Vec::new(),
            cached_blocks: 0,
            cache_width: 0,
            cache_last_dense: false,
            scroll: Scroll::new(),
            viewport_h: 0,
        }
    }

    fn push_raw(&mut self, raw: &str) {
        if let Ok(row) = serde_json::from_str::<Value>(raw) {
            self.chrome.update(&row);
        }
        self.finalized.extend(self.parser.push(raw));
    }

    /// Finalized cache (rebuilt on width change) + freshly rendered pending.
    fn scrollback_lines(&mut self, inner_w: usize) -> Vec<Line<'static>> {
        if self.cache_width != inner_w {
            self.cache.clear();
            self.cached_blocks = 0;
            self.cache_last_dense = false;
            self.cache_width = inner_w;
        }
        while self.cached_blocks < self.finalized.len() {
            let block = self.finalized[self.cached_blocks].clone();
            let mut dense = self.cache_last_dense;
            push_block(self.theme, &mut self.cache, &block, inner_w, &mut dense);
            self.cache_last_dense = dense;
            self.cached_blocks += 1;
        }
        let mut lines = self.cache.clone();
        let mut last_dense = self.cache_last_dense;
        for block in self.parser.pending_blocks() {
            push_block(self.theme, &mut lines, &block, inner_w, &mut last_dense);
        }
        lines
    }
}

fn top_line(app: &App, inner_w: usize) -> Line<'static> {
    let t = app.theme;
    let right = if app.chrome.context_used > 0 {
        let used = app.chrome.context_used;
        let total = if used > CONTEXT_TOTAL {
            CONTEXT_TOTAL_LARGE
        } else {
            CONTEXT_TOTAL
        };
        let mut text = format!("{} / {}", fmt_tokens(used), fmt_tokens(total));
        while UnicodeWidthStr::width(text.as_str()) < 6 {
            text.push(' ');
        }
        let pct = used as f64 / total as f64 * 100.0;
        Some((text, t.usage_color(pct)))
    } else {
        None
    };
    let right_w = right
        .as_ref()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .unwrap_or(0);
    let budget = if right_w > 0 {
        inner_w.saturating_sub(right_w + 1)
    } else {
        inner_w
    };
    let branch = app
        .chrome
        .branch
        .clone()
        .unwrap_or_else(|| "detached".to_string());
    let cwd = app
        .chrome
        .cwd
        .as_deref()
        .map(abbreviate_path)
        .unwrap_or_default();
    let left = vec![
        Span::styled(
            format!("⎇ {branch}"),
            fg(t.text_primary).add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled("worktree ", fg(t.accent_user)),
        Span::styled(cwd, fg(t.gray_dim)),
    ];
    let mut line = clip_spans(left, budget);
    if let Some((text, color)) = right {
        let left_w = cells_width(&line_cells(&line));
        let pad = inner_w.saturating_sub(left_w + right_w);
        line.spans.push(Span::raw(" ".repeat(pad)));
        line.spans.push(Span::styled(text, fg(color)));
    }
    line
}

fn bottom_line(t: &ViewTheme, model: Option<&str>, inner_w: usize) -> Line<'static> {
    let key = fg(t.text_secondary).add_modifier(Modifier::BOLD);
    let label = fg(t.gray);
    let sep = fg(t.gray).add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(m) = model {
        spans.push(Span::styled(m.to_string(), fg(t.accent_model)));
        spans.push(Span::styled(" · ", sep));
    }
    spans.push(Span::styled("read-only mirror", label));
    spans.push(Span::styled(" · ", sep));
    spans.push(Span::styled("q", key));
    spans.push(Span::styled(":quit ", label));
    spans.push(Span::styled("j", key));
    spans.push(Span::styled("/", label));
    spans.push(Span::styled("k", key));
    spans.push(Span::styled(":scroll ", label));
    spans.push(Span::styled("G", key));
    spans.push(Span::styled(":follow", label));
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    let w = cells_width(&line_cells(&line));
    if w > inner_w {
        return clip_spans(line.spans, inner_w);
    }
    let mut spans = line.spans;
    spans.insert(0, Span::raw(" ".repeat(inner_w - w)));
    Line::from(spans)
}

fn draw(frame: &mut Frame, app: &mut App) {
    let t = app.theme;
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(t.bg_base)), area);
    if area.width < 10 || area.height < 4 {
        return;
    }
    let compact = area.height <= 20;
    let hpad: u16 = if compact { 1 } else { 2 };
    let vpad: u16 = if compact { 0 } else { 1 };
    let inner = Rect {
        x: area.x + hpad,
        y: area.y + vpad,
        width: area.width.saturating_sub(hpad * 2),
        height: area.height.saturating_sub(vpad * 2),
    };
    let gap: u16 = if vpad > 0 { 1 } else { 0 };
    let status_rect = Rect { height: 1, ..inner };
    let hint_rect = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    let scroll_h = inner.height.saturating_sub(2 + gap);
    let scroll_rect = Rect {
        x: inner.x,
        y: inner.y + 1 + gap,
        width: inner.width,
        height: scroll_h,
    };

    let base = Style::default().bg(t.bg_base).fg(t.text_primary);
    let lines = app.scrollback_lines(inner.width as usize);
    app.viewport_h = scroll_h as usize;
    app.scroll
        .sync(lines.len().saturating_sub(scroll_h as usize));
    let end = (app.scroll.offset + scroll_h as usize).min(lines.len());
    let visible: Vec<Line> = lines[app.scroll.offset..end].to_vec();
    frame.render_widget(Paragraph::new(visible).style(base), scroll_rect);
    frame.render_widget(
        Paragraph::new(top_line(app, inner.width as usize)).style(base),
        status_rect,
    );
    frame.render_widget(
        Paragraph::new(bottom_line(t, app.parser.model(), inner.width as usize)).style(base),
        hint_rect,
    );
}

// ---------------------------------------------------------------------------
// Terminal lifecycle & event loop
// ---------------------------------------------------------------------------

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Run the TUI mirror over `path`. Only call with stdout on a tty.
pub fn run(path: &Path) -> anyhow::Result<i32> {
    // Resolve the theme BEFORE crossterm owns the terminal: in auto mode the
    // OSC 11 background probe writes/reads the raw tty itself.
    let theme = crate::view_theme::active_theme_kind().theme();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut backlog = String::new();
    reader.read_to_string(&mut backlog)?;
    let mut app = App::new(theme);
    {
        let lines: Vec<&str> = backlog.lines().collect();
        for raw in &lines[lines.len().saturating_sub(TAIL_EVENTS)..] {
            app.push_raw(raw);
        }
    }

    enable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut terminal, &mut app, &mut reader);
    restore_terminal();
    result.map(|_| 0)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    reader: &mut BufReader<File>,
) -> anyhow::Result<()> {
    let mut raw = String::new();
    loop {
        loop {
            raw.clear();
            match reader.read_line(&mut raw) {
                Ok(0) => break,
                Ok(_) => app.push_raw(&raw),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => break,
                Err(err) => return Err(err.into()),
            }
        }
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(POLL_MS))? {
            continue;
        }
        loop {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if handle_key(&mut app.scroll, app.viewport_h, k.code, k.modifiers) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => handle_mouse(&mut app.scroll, m.kind),
                _ => {}
            }
            if !event::poll(Duration::from_millis(0))? {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_theme::{GROKDAY, GROKNIGHT};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use serde_json::json;

    const W: u16 = 80;
    const H: u16 = 30;

    fn draw_to_buffer(app: &mut App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect()
    }

    fn buffer_text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn user_row(text: &str) -> String {
        json!({
            "type": "user", "gitBranch": "rs-rewrite", "cwd": "/Users/dp/dev/hive",
            "timestamp": "2026-08-30T12:40:00.000Z",
            "message": {"content": text},
        })
        .to_string()
    }

    fn assistant_text_row(text: &str) -> String {
        json!({
            "type": "assistant", "gitBranch": "rs-rewrite", "cwd": "/Users/dp/dev/hive",
            "timestamp": "2026-08-30T12:44:06.000Z",
            "message": {
                "model": "claude-fable-5",
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": 60_000, "cache_read_input_tokens": 9_000,
                          "output_tokens": 0},
            },
        })
        .to_string()
    }

    fn tool_use_row(name: &str, id: &str, input: serde_json::Value) -> String {
        json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]},
        })
        .to_string()
    }

    #[test]
    fn test_top_line_renders_branch_worktree_cwd_and_token_counter() {
        std::env::set_var("HOME", "/Users/dp");
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("hello"));
        app.push_raw(&assistant_text_row("done"));
        let buf = draw_to_buffer(&mut app, W, H);
        let top = row_text(&buf, 1);
        assert!(top.contains("⎇ rs-rewrite"), "{top:?}");
        assert!(top.contains("worktree ~/dev/hive"), "{top:?}");
        assert!(top.contains("69K / 200K"), "{top:?}");
        // Right cluster ends flush at the inner right edge (x = W-3 last col).
        assert_eq!(&top[top.len() - 2..], "  ");
        assert!(top.trim_end().ends_with("200K"), "{top:?}");
    }

    #[test]
    fn test_user_band_fills_full_inner_width_with_cjk_text() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row(
            "宽度测试中文段落，一直排到需要换行为止，确认背景条完整。",
        ));
        let buf = draw_to_buffer(&mut app, W, H);
        let band_rows: Vec<u16> = (0..H)
            .filter(|&y| {
                buf.cell((2, y))
                    .is_some_and(|c| c.style().bg == Some(GROKNIGHT.bg_light))
            })
            .collect();
        assert!(band_rows.len() >= 3, "vpad + content rows: {band_rows:?}");
        let prompt_row = (0..H).find(|&y| row_text(&buf, y).contains('❯')).unwrap();
        assert!(band_rows.contains(&prompt_row));
        assert!(band_rows.contains(&(prompt_row - 1)), "vpad above");
        for &y in &band_rows {
            // Wide glyphs leave a reset shadow cell behind them in the
            // buffer (the glyph itself paints both columns), so walk by
            // display width instead of asserting every raw cell.
            let mut x: u16 = 2;
            while x < W - 2 {
                let cell = buf.cell((x, y)).unwrap();
                assert_eq!(
                    cell.style().bg,
                    Some(GROKNIGHT.bg_light),
                    "band bg must span the inner width at ({x},{y})"
                );
                x += UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
            }
            assert_eq!(
                buf.cell((0, y)).unwrap().style().bg,
                Some(GROKNIGHT.bg_base)
            );
            assert_eq!(
                buf.cell((W - 1, y)).unwrap().style().bg,
                Some(GROKNIGHT.bg_base)
            );
        }
        // Timestamp overlays the first band line, right-aligned.
        let first = row_text(&buf, prompt_row);
        assert!(first.contains("12:40 PM"), "{first:?}");
    }

    #[test]
    fn test_grokday_theme_paints_light_frame_and_band() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKDAY);
        app.push_raw(&user_row("hello light"));
        app.push_raw(&assistant_text_row("done"));
        let buf = draw_to_buffer(&mut app, W, H);
        // Frame fill is the grokday base, not the dark one.
        assert_eq!(
            buf.cell((0, 0)).unwrap().style().bg,
            Some(Color::Rgb(238, 238, 238))
        );
        // User band uses the grokday highlight bg with dark primary text.
        let prompt_row = (0..H).find(|&y| row_text(&buf, y).contains('❯')).unwrap();
        let band_cell = buf.cell((2, prompt_row)).unwrap();
        assert_eq!(band_cell.style().bg, Some(Color::Rgb(222, 222, 222)));
        let prompt_x = (0..W)
            .find(|&x| buf.cell((x, prompt_row)).unwrap().symbol() == "❯")
            .unwrap();
        let body_cell = buf.cell((prompt_x + 2, prompt_row)).unwrap();
        assert_eq!(body_cell.style().fg, Some(Color::Rgb(38, 38, 38)));
    }

    #[test]
    fn test_tool_lines_render_group_run_and_thinking() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&tool_use_row("Read", "t1", json!({"file_path": "/a.rs"})));
        app.push_raw(&tool_use_row("Grep", "t2", json!({"pattern": "fn main"})));
        app.push_raw(&tool_use_row(
            "Bash",
            "t3",
            json!({"command": "ls", "description": "List files"}),
        ));
        app.push_raw(
            &json!({
                "type": "assistant", "timestamp": "2026-08-30T12:40:14.300Z",
                "message": {"content": [{"type": "thinking", "thinking": "hmm"}]},
            })
            .to_string(),
        );
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("◈ Read 1 file, Searched 1 pattern"), "{text}");
        assert!(text.contains("◆ Run List files"), "{text}");
        assert!(text.contains("◆ Thought for 14.3s"), "{text}");
    }

    #[test]
    fn test_bottom_line_is_right_aligned_muted_hint() {
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&assistant_text_row("hi"));
        let buf = draw_to_buffer(&mut app, W, H);
        let bottom = row_text(&buf, H - 2);
        let trimmed = bottom.trim_end();
        assert!(
            trimmed.ends_with("claude-fable-5 · read-only mirror · q:quit j/k:scroll G:follow"),
            "{bottom:?}"
        );
        // Right-aligned: content hugs the inner right edge (one trailing space
        // inside the row, then the 2-col outer pad).
        let pad = bottom.len() - trimmed.len();
        assert!(pad <= 3, "{bottom:?}");
        assert!(bottom.starts_with(' '), "{bottom:?}");
    }

    #[test]
    fn test_worked_for_line_and_hive_envelope_head() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&assistant_text_row("done"));
        let envelope = "<HIVE from=comb.dodo to=comb.rex msgId=a1>review the spec</HIVE>";
        app.push_raw(&user_row(envelope));
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("Worked for 4m6s"), "{text}");
        assert!(text.contains("❯ <HIVE from=comb.dodo"), "{text}");
        assert!(text.contains("review the spec"), "{text}");
    }

    #[test]
    fn test_fmt_tokens_matches_grok_buckets() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2K");
        assert_eq!(fmt_tokens(69_900), "69K");
        assert_eq!(fmt_tokens(500_000), "500K");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
        assert_eq!(fmt_tokens(12_000_000), "12M");
    }

    #[test]
    fn test_wrap_plain_never_straddles_wide_chars() {
        let lines = wrap_plain("宽宽宽宽宽", 5);
        assert_eq!(lines, vec!["宽宽", "宽宽", "宽"]);
        let lines = wrap_plain("one two three", 7);
        assert_eq!(lines, vec!["one two", "three"]);
    }

    // ---- scroll/follow state machine (grok nav.rs semantics) -----------

    fn scroll_at(offset: usize, max: usize, follow: bool) -> Scroll {
        Scroll {
            offset,
            max,
            follow,
        }
    }

    #[test]
    fn test_scroll_up_disengages_follow() {
        let mut s = scroll_at(50, 50, true);
        s.scroll_up(1);
        assert!(!s.follow);
        assert_eq!(s.offset, 49);
        s.scroll_up(100);
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn test_scroll_down_landing_at_bottom_does_not_follow() {
        // A scroll that merely reaches the bottom is not an overscroll.
        let mut s = scroll_at(47, 50, false);
        s.scroll_down(3);
        assert_eq!(s.offset, 50);
        assert!(!s.follow, "landing exactly at max must not re-engage");
        // The next down gesture moves zero rows → overscroll → follow.
        s.scroll_down(1);
        assert!(s.follow);
    }

    #[test]
    fn test_scroll_down_mid_buffer_keeps_follow_off() {
        let mut s = scroll_at(10, 50, false);
        s.scroll_down(3);
        assert_eq!(s.offset, 13);
        assert!(!s.follow);
        s.scroll_down(0);
        assert!(!s.follow, "zero-row scroll is not a gesture");
    }

    #[test]
    fn test_goto_top_and_bottom() {
        let mut s = scroll_at(25, 50, true);
        s.goto_top();
        assert_eq!(s.offset, 0);
        assert!(!s.follow);
        s.goto_bottom();
        assert_eq!(s.offset, 50);
        assert!(s.follow);
    }

    #[test]
    fn test_sync_pins_to_bottom_only_while_following() {
        let mut s = scroll_at(50, 50, true);
        s.sync(60); // new transcript lines arrived
        assert_eq!(s.offset, 60, "follow tails the file");
        s.scroll_up(5);
        s.sync(70);
        assert_eq!(s.offset, 55, "detached offset holds its place");
        s.sync(40); // width change shrank the line count
        assert_eq!(s.offset, 40, "offset clamps to the new max");
    }

    #[test]
    fn test_page_and_half_page_rows_match_grok() {
        assert_eq!(page_rows(30), 28); // viewport − 2 overlap
        assert_eq!(page_rows(2), 1); // min 1
        assert_eq!(page_rows(0), 1);
        assert_eq!(half_page_rows(30), 15);
        assert_eq!(half_page_rows(1), 1);
        assert_eq!(half_page_rows(0), 1);
    }

    #[test]
    fn test_handle_key_quit_bindings() {
        let mut s = scroll_at(0, 50, true);
        assert!(handle_key(
            &mut s,
            30,
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ));
        assert!(handle_key(
            &mut s,
            30,
            KeyCode::Char('q'),
            KeyModifiers::CONTROL
        ));
        assert!(handle_key(
            &mut s,
            30,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ));
        assert!(!handle_key(
            &mut s,
            30,
            KeyCode::Char('x'),
            KeyModifiers::NONE
        ));
    }

    #[test]
    fn test_handle_key_line_page_and_jump_bindings() {
        let mut s = scroll_at(20, 50, false);
        assert!(!handle_key(
            &mut s,
            30,
            KeyCode::Char('j'),
            KeyModifiers::NONE
        ));
        assert_eq!(s.offset, 21);
        handle_key(&mut s, 30, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(s.offset, 22);
        handle_key(&mut s, 30, KeyCode::Char('k'), KeyModifiers::NONE);
        handle_key(&mut s, 30, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.offset, 20);
        // Ctrl+J / Ctrl+K single-line scroll.
        handle_key(&mut s, 30, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 21);
        handle_key(&mut s, 30, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 20);
        // Ctrl+D / Ctrl+U half page (viewport 30 → 15).
        handle_key(&mut s, 30, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 35);
        handle_key(&mut s, 30, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 20);
        // PageDown / PageUp (viewport 30 → 28).
        handle_key(&mut s, 30, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(s.offset, 48);
        handle_key(&mut s, 30, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(s.offset, 20);
        // g / G jumps (G also re-engages follow).
        handle_key(&mut s, 30, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(s.offset, 0);
        assert!(!s.follow);
        handle_key(&mut s, 30, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(s.offset, 50);
        assert!(s.follow);
    }

    #[test]
    fn test_j_at_bottom_reengages_follow() {
        // grok selection.rs: a single j on the last entry enters follow.
        let mut s = scroll_at(50, 50, false);
        handle_key(&mut s, 30, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(s.follow);
    }

    #[test]
    fn test_mouse_wheel_scrolls_three_lines() {
        let mut s = scroll_at(20, 50, true);
        handle_mouse(&mut s, MouseEventKind::ScrollUp);
        assert_eq!(s.offset, 17);
        assert!(!s.follow, "wheel up leaves follow");
        handle_mouse(&mut s, MouseEventKind::ScrollDown);
        assert_eq!(s.offset, 20);
        assert!(!s.follow);
        // At the bottom, one more wheel-down tick re-engages follow.
        s.offset = 50;
        handle_mouse(&mut s, MouseEventKind::ScrollDown);
        assert!(s.follow);
    }
}
