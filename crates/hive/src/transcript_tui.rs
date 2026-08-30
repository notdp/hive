//! Ratatui TUI for `hive view` — the Grok Build look (groknight palette) over
//! the [`crate::transcript_view`] display-block model.
//!
//! Chrome: full-screen `#141414` fill, 2-col outer inset, top status line
//! (branch / worktree / `~`-abbreviated cwd, right-aligned token counter),
//! scrollback (gray full-width user bands, `◈`/`◆` tool lines, grok-markdown
//! assistant text with right-aligned timestamps, muted `Worked for …`), and a
//! single muted bottom hint row. Read-only: keys only quit and scroll.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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

// groknight palette (grok-build xai-grok-pager-render/src/theme/groknight.rs).
const BG_BASE: Color = Color::Rgb(20, 20, 20);
const BG_BAND: Color = Color::Rgb(36, 36, 36);
const FG_TEXT: Color = Color::Rgb(225, 225, 225);
const FG_SECONDARY: Color = Color::Rgb(200, 200, 200);
const GRAY: Color = Color::Rgb(108, 108, 108);
const GRAY_DIM: Color = Color::Rgb(88, 88, 88);
const GRAY_BRIGHT: Color = Color::Rgb(120, 120, 120);
const RED: Color = Color::Rgb(247, 118, 142);
const MODEL_TEAL: Color = Color::Rgb(26, 188, 156);

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

/// Usage gradient: #e1e1e1 → #c8c8c8 (50-65) → #e0af68 (75-85) → #f7768e (95+).
fn token_color(pct: f64) -> Color {
    const STOPS: [(f64, (u8, u8, u8)); 7] = [
        (0.0, (225, 225, 225)),
        (50.0, (200, 200, 200)),
        (65.0, (200, 200, 200)),
        (75.0, (224, 175, 104)),
        (85.0, (224, 175, 104)),
        (95.0, (247, 118, 142)),
        (100.0, (247, 118, 142)),
    ];
    let pct = pct.clamp(0.0, 100.0);
    for pair in STOPS.windows(2) {
        let (p0, c0) = pair[0];
        let (p1, c1) = pair[1];
        if pct <= p1 {
            let t = if p1 > p0 { (pct - p0) / (p1 - p0) } else { 0.0 };
            let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
            return Color::Rgb(lerp(c0.0, c1.0), lerp(c0.1, c1.1), lerp(c0.2, c1.2));
        }
    }
    let (_, c) = STOPS[STOPS.len() - 1];
    Color::Rgb(c.0, c.1, c.2)
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
fn band_line(spans: Vec<Span<'static>>, inner_w: usize) -> Line<'static> {
    let band = Style::default().bg(BG_BAND);
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

fn push_user_block(out: &mut Vec<Line<'static>>, u: &UserBlock, inner_w: usize) {
    let band = Style::default().bg(BG_BAND);
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
    out.push(band_line(Vec::new(), inner_w)); // vpad top
    for (i, text) in vis.iter().enumerate() {
        let prefix = if i == 0 { "❯ " } else { "  " };
        let mut spans = vec![
            Span::styled(prefix.to_string(), fg(FG_SECONDARY).bg(BG_BAND)),
            Span::styled(text.clone(), fg(FG_TEXT).bg(BG_BAND)),
        ];
        if i == 0 {
            if let Some(ts) = u.timestamp.as_ref() {
                let used = 2 + UnicodeWidthStr::width(text.as_str());
                let ts_text = format!("  {}", ts.clock);
                let ts_w = UnicodeWidthStr::width(ts_text.as_str());
                if cw > used + ts_w {
                    spans.push(Span::styled(" ".repeat(cw - used - ts_w), band));
                    spans.push(Span::styled(ts_text, fg(GRAY).bg(BG_BAND)));
                }
            }
        }
        out.push(band_line(spans, inner_w));
    }
    out.push(band_line(Vec::new(), inner_w)); // vpad bottom
}

fn push_assistant_block(out: &mut Vec<Line<'static>>, a: &AssistantBlock, inner_w: usize) {
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let aw = cw.saturating_sub(TS_RESERVE).max(10);
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in grok_md::render_ratatui(&a.markdown) {
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
                    spans.push(Span::styled(ts_text, fg(GRAY)));
                }
            }
        }
        out.push(Line::from(spans));
    }
}

fn thinking_spans(t: &ThinkingBlock) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled("◆ ", fg(GRAY)),
        Span::styled("Thought", fg(GRAY).add_modifier(Modifier::BOLD)),
    ];
    if let Some(secs) = t.duration_secs {
        spans.push(Span::styled(
            format!(
                " for {}",
                crate::transcript_view::format_thinking_duration(secs)
            ),
            fg(GRAY),
        ));
    }
    spans
}

fn push_block(
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
        DisplayBlock::User(u) => push_user_block(out, u, inner_w),
        DisplayBlock::Assistant(a) => push_assistant_block(out, a, inner_w),
        DisplayBlock::ToolGroup(g) => {
            let failed = g.failed();
            let bullet = if failed > 0 { RED } else { GRAY };
            let mut spans = vec![
                margin,
                Span::styled("◈ ", fg(bullet)),
                Span::styled(g.label(), fg(GRAY_BRIGHT).add_modifier(Modifier::BOLD)),
            ];
            if failed > 0 {
                spans.push(Span::styled(format!(" · {failed} failed"), fg(RED)));
            }
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Run(r) => {
            let err = r.result.as_ref().is_some_and(|res| res.is_error);
            let bullet = if err { RED } else { GRAY };
            let spans = vec![
                margin,
                Span::styled("◆ ", fg(bullet)),
                Span::styled("Run ", fg(GRAY).add_modifier(Modifier::BOLD)),
                Span::styled(r.description.clone(), fg(GRAY)),
            ];
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Tool(t) => {
            let err = t.result.as_ref().is_some_and(|res| res.is_error);
            let bullet = if err { RED } else { GRAY };
            let mut spans = vec![
                margin,
                Span::styled("◆ ", fg(bullet)),
                Span::styled(t.name.clone(), fg(GRAY).add_modifier(Modifier::BOLD)),
            ];
            if !t.hint.is_empty() {
                spans.push(Span::styled(format!("  {}", t.hint), fg(GRAY)));
            }
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::Thinking(t) => {
            let mut spans = vec![margin];
            spans.extend(thinking_spans(t));
            out.push(clip_spans(spans, inner_w));
        }
        DisplayBlock::WorkedFor(w) => {
            let spans = vec![margin, Span::styled(w.label(), fg(GRAY))];
            out.push(clip_spans(spans, inner_w));
        }
    }
}

// ---------------------------------------------------------------------------
// App state & frame chrome
// ---------------------------------------------------------------------------

struct App {
    parser: TranscriptParser,
    chrome: Chrome,
    finalized: Vec<DisplayBlock>,
    cache: Vec<Line<'static>>,
    cached_blocks: usize,
    cache_width: usize,
    cache_last_dense: bool,
    scroll: usize,
    max_scroll: usize,
    follow: bool,
}

impl App {
    fn new() -> Self {
        App {
            parser: TranscriptParser::new(),
            chrome: Chrome::default(),
            finalized: Vec::new(),
            cache: Vec::new(),
            cached_blocks: 0,
            cache_width: 0,
            cache_last_dense: false,
            scroll: 0,
            max_scroll: 0,
            follow: true,
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
            push_block(&mut self.cache, &block, inner_w, &mut dense);
            self.cache_last_dense = dense;
            self.cached_blocks += 1;
        }
        let mut lines = self.cache.clone();
        let mut last_dense = self.cache_last_dense;
        for block in self.parser.pending_blocks() {
            push_block(&mut lines, &block, inner_w, &mut last_dense);
        }
        lines
    }
}

fn top_line(app: &App, inner_w: usize) -> Line<'static> {
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
        Some((text, token_color(pct)))
    } else {
        None
    };
    let right_w = right
        .as_ref()
        .map(|(t, _)| UnicodeWidthStr::width(t.as_str()))
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
            fg(FG_TEXT).add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled("worktree ", fg(FG_SECONDARY)),
        Span::styled(cwd, fg(GRAY_DIM)),
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

fn bottom_line(model: Option<&str>, inner_w: usize) -> Line<'static> {
    let key = fg(FG_SECONDARY).add_modifier(Modifier::BOLD);
    let label = fg(GRAY);
    let sep = fg(GRAY).add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(m) = model {
        spans.push(Span::styled(m.to_string(), fg(MODEL_TEAL)));
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
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG_BASE)), area);
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

    let base = Style::default().bg(BG_BASE).fg(FG_TEXT);
    let lines = app.scrollback_lines(inner.width as usize);
    app.max_scroll = lines.len().saturating_sub(scroll_h as usize);
    if app.follow || app.scroll > app.max_scroll {
        app.scroll = app.max_scroll;
    }
    let end = (app.scroll + scroll_h as usize).min(lines.len());
    let visible: Vec<Line> = lines[app.scroll..end].to_vec();
    frame.render_widget(Paragraph::new(visible).style(base), scroll_rect);
    frame.render_widget(
        Paragraph::new(top_line(app, inner.width as usize)).style(base),
        status_rect,
    );
    frame.render_widget(
        Paragraph::new(bottom_line(app.parser.model(), inner.width as usize)).style(base),
        hint_rect,
    );
}

// ---------------------------------------------------------------------------
// Terminal lifecycle & event loop
// ---------------------------------------------------------------------------

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Run the TUI mirror over `path`. Only call with stdout on a tty.
pub fn run(path: &Path) -> anyhow::Result<i32> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut backlog = String::new();
    reader.read_to_string(&mut backlog)?;
    let mut app = App::new();
    {
        let lines: Vec<&str> = backlog.lines().collect();
        for raw in &lines[lines.len().saturating_sub(TAIL_EVENTS)..] {
            app.push_raw(raw);
        }
    }

    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;
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
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Release {
                    match (k.code, k.modifiers) {
                        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        (KeyCode::Char('q'), _) => return Ok(()),
                        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                            app.scroll = (app.scroll + 1).min(app.max_scroll);
                            app.follow = app.scroll >= app.max_scroll;
                        }
                        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                            app.follow = false;
                            app.scroll = app.scroll.saturating_sub(1);
                        }
                        (KeyCode::Char('G'), _) => app.follow = true,
                        _ => {}
                    }
                }
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
        let mut app = App::new();
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
        let mut app = App::new();
        app.push_raw(&user_row(
            "宽度测试中文段落，一直排到需要换行为止，确认背景条完整。",
        ));
        let buf = draw_to_buffer(&mut app, W, H);
        let band_rows: Vec<u16> = (0..H)
            .filter(|&y| {
                buf.cell((2, y))
                    .is_some_and(|c| c.style().bg == Some(BG_BAND))
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
                    Some(BG_BAND),
                    "band bg must span the inner width at ({x},{y})"
                );
                x += UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
            }
            assert_eq!(buf.cell((0, y)).unwrap().style().bg, Some(BG_BASE));
            assert_eq!(buf.cell((W - 1, y)).unwrap().style().bg, Some(BG_BASE));
        }
        // Timestamp overlays the first band line, right-aligned.
        let first = row_text(&buf, prompt_row);
        assert!(first.contains("12:40 PM"), "{first:?}");
    }

    #[test]
    fn test_tool_lines_render_group_run_and_thinking() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new();
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
        let mut app = App::new();
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
        let mut app = App::new();
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
}
