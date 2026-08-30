//! Ratatui TUI for `hive view` — the Grok Build look over the
//! [`crate::transcript_view`] display-block model, in the resolved
//! [`crate::view_theme`] (grokday light / groknight dark).
//!
//! Chrome: full-screen bg fill, 2-col outer inset, top status line
//! (branch / worktree / `~`-abbreviated cwd, right-aligned token counter),
//! scrollback (full-width user bands, `◈`/`◆` tool lines, grok-markdown
//! assistant text with right-aligned timestamps, muted `Worked for …`),
//! grok's rounded read-only composer box (`╭─╮ │ ❯ │ ╰─╯`), and the muted
//! hint row below it.
//!
//! Interaction layer (grok's pager surface, read-only): Up/Down block
//! selection with grok's bracket-frame highlight, Shift+Left/Right turn
//! jumps, Left/Right (and double-click) per-block collapse/expand, Ctrl+E
//! all-thinking toggle, Ctrl+O density cycle (normal/thinking/verbose),
//! Enter/Ctrl+F full-screen block viewer, and a `/` command palette
//! (/theme /view /find /quit) whose input types into the composer box, the
//! dropdown anchored above it. Keystrokes still go nowhere by construction —
//! the mirror only ever reads the transcript.

mod interact;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::transcript_view::{
    grok_md, hive_envelope, AssistantBlock, DisplayBlock, Entry, RunBlock, ThinkingBlock,
    ToolBlock, ToolGroupBlock, ToolOutcome, TranscriptParser, UserBlock,
};
use crate::view_theme::{ThemeKind, ThemePref, ViewTheme};
use interact::{
    EntryInfo, FoldKind, FoldState, Palette, PaletteAction, SelectMove, MAX_PALETTE_ROWS,
    PALETTE_COMMANDS,
};

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
/// Same-block double-click window (grok MULTI_CLICK_TIMEOUT_MS).
const MULTI_CLICK: Duration = Duration::from_millis(300);
/// Thinking-body strength between bg and text (grok thinking.rs bg_blend).
const THINKING_BLEND: f64 = 0.7;

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

/// Per-channel lerp from `from` toward `to` (grok blend_color).
fn blend(from: Color, to: Color, factor: f64) -> Color {
    let ch = |c: Color| match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    let (fr, fgc, fb) = ch(from);
    let (tr, tgc, tb) = ch(to);
    let l = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * factor).round() as u8;
    Color::Rgb(l(fr, tr), l(fgc, tgc), l(fb, tb))
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

/// grok's selected-collapsed-header patch: fill the content columns
/// (inset 1 col from the selection border on each side) with `bg`.
fn patch_bg(line: Line<'static>, inner_w: usize, bg: Color) -> Line<'static> {
    let mut cells = line_cells(&line);
    let content_end = inner_w.saturating_sub(2);
    let mut w = cells_width(&cells);
    while w < content_end {
        cells.push((' ', Style::default()));
        w += 1;
    }
    let mut col = 0usize;
    for cell in cells.iter_mut() {
        let cw = cell_width(cell.0);
        if col >= 3 && col + cw <= content_end {
            cell.1.bg = Some(bg);
        }
        col += cw;
    }
    cells_to_line(&cells)
}

/// One rendered entry plus whether Left/Right can fold it at all.
struct Rendered {
    lines: Vec<Line<'static>>,
    foldable: bool,
}

fn render_user(t: &ViewTheme, u: &UserBlock, inner_w: usize, expanded: bool) -> Rendered {
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
    let foldable = vis.len() > BAND_MAX_LINES;
    if foldable && !expanded {
        vis.truncate(BAND_MAX_LINES);
        let last = vis.pop().unwrap_or_default();
        vis.push(format!("{} …", clip_plain(&last, bw.saturating_sub(2))));
    }
    let mut out = Vec::new();
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
    Rendered {
        lines: out,
        foldable,
    }
}

fn render_assistant(t: &ViewTheme, a: &AssistantBlock, inner_w: usize) -> Rendered {
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let aw = cw.saturating_sub(TS_RESERVE).max(10);
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in grok_md::render_ratatui(&a.markdown, t, aw) {
        wrapped.extend(wrap_line(&line, aw));
    }
    while wrapped.last().is_some_and(|l| line_cells(l).is_empty()) {
        wrapped.pop();
    }
    let mut out = Vec::new();
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
    Rendered {
        lines: out,
        foldable: false,
    }
}

/// Thinking body line at 70% strength between bg and its own colors.
fn blend_thinking_line(line: Line<'static>, t: &ViewTheme) -> Line<'static> {
    let mut cells = line_cells(&line);
    for cell in cells.iter_mut() {
        let base = cell.1.fg.unwrap_or(t.md_text);
        cell.1.fg = Some(blend(t.bg_base, base, THINKING_BLEND));
    }
    cells_to_line(&cells)
}

fn render_thinking(
    t: &ViewTheme,
    tb: &ThinkingBlock,
    inner_w: usize,
    expanded: bool,
    selected: bool,
) -> Rendered {
    let collapsed_sel = selected && !expanded;
    let bullet = if collapsed_sel { "› " } else { "◆ " };
    let label_style = if collapsed_sel {
        bold(t.text_primary)
    } else {
        bold(t.gray)
    };
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(bullet, fg(t.gray)),
        Span::styled("Thought", label_style),
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
    let mut header = clip_spans(spans, inner_w);
    if collapsed_sel {
        header = patch_bg(header, inner_w, t.bg_dark);
    }
    let mut lines = vec![header];
    if expanded {
        let bw = inner_w.saturating_sub(ROW_CHROME);
        let mut body: Vec<Line<'static>> = Vec::new();
        for line in grok_md::render_ratatui(&tb.text, t, bw) {
            body.extend(wrap_line(&line, bw));
        }
        while body.last().is_some_and(|l| line_cells(l).is_empty()) {
            body.pop();
        }
        for line in body {
            let blended = blend_thinking_line(line, t);
            let mut spans = vec![Span::styled("│", fg(t.accent_thinking)), Span::raw("  ")];
            spans.extend(blended.spans);
            lines.push(Line::from(spans));
        }
    }
    Rendered {
        lines,
        foldable: true,
    }
}

/// One tool-output band row (grok execute.rs `.with_panel_background`): the
/// text on the full content-width `bg_dark` band.
fn band_row(t: &ViewTheme, text: String, text_fg: Color, inner_w: usize) -> Line<'static> {
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let used = UnicodeWidthStr::width(text.as_str());
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(text, fg(text_fg).bg(t.bg_dark)),
    ];
    if cw > used {
        spans.push(Span::styled(
            " ".repeat(cw - used),
            Style::default().bg(t.bg_dark),
        ));
    }
    Line::from(spans)
}

/// Expanded tool output (grok execute.rs render_with_truncation): one blank
/// spacer, then — success — full-width `bg_dark` band rows in primary text
/// preserving line breaks, or — error — accent_error text with no band
/// (grok's error branch). The storage-cap marker renders muted.
fn outcome_rows(t: &ViewTheme, out: &mut Vec<Line<'static>>, res: &ToolOutcome, inner_w: usize) {
    let pw = inner_w.saturating_sub(ROW_CHROME);
    let text = res.text.trim_end();
    if text.is_empty() && !res.truncated {
        return;
    }
    out.push(Line::default());
    if res.is_error {
        for src in text.lines() {
            for piece in wrap_plain(src, pw) {
                out.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(piece, fg(t.accent_error)),
                ]));
            }
        }
        if res.truncated {
            out.push(Line::from(vec![
                Span::raw("   "),
                Span::styled("… output truncated".to_string(), fg(t.gray)),
            ]));
        }
        return;
    }
    for src in text.lines() {
        for piece in wrap_plain(src, pw) {
            out.push(band_row(t, piece, t.text_primary, inner_w));
        }
    }
    if res.truncated {
        out.push(band_row(
            t,
            "… output truncated".to_string(),
            t.gray,
            inner_w,
        ));
    }
}

/// Expanded Run command lines (grok execute.rs push_command_soft_wrap): `$ `
/// in gray_dim, the command body in the theme's bash function-call blue,
/// physical newlines preserved, soft-wrap and continuation rows hanging
/// 2 cols so they align under the command after the `$ `.
fn command_rows(t: &ViewTheme, out: &mut Vec<Line<'static>>, command: &str, inner_w: usize) {
    let bw = inner_w.saturating_sub(ROW_CHROME + 2).max(1);
    let mut first = true;
    for src in command.lines() {
        for piece in wrap_plain(src, bw) {
            let lead = if first { "$ " } else { "  " };
            first = false;
            out.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(lead.to_string(), fg(t.gray_dim)),
                Span::styled(piece, fg(t.command_fg)),
            ]));
        }
    }
}

fn render_group(
    t: &ViewTheme,
    g: &ToolGroupBlock,
    inner_w: usize,
    expanded: bool,
    selected: bool,
) -> Rendered {
    let collapsed_sel = selected && !expanded;
    let failed = g.failed();
    let bullet_color = if failed > 0 { t.accent_error } else { t.gray };
    let bullet = if collapsed_sel { "› " } else { "◈ " };
    let label_style = if collapsed_sel {
        bold(t.text_primary)
    } else {
        bold(t.gray_bright)
    };
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(bullet, fg(bullet_color)),
        Span::styled(g.label(), label_style),
    ];
    if failed > 0 {
        spans.push(Span::styled(
            format!(" · {failed} failed"),
            fg(t.accent_error),
        ));
    }
    let mut header = clip_spans(spans, inner_w);
    if collapsed_sel {
        header = patch_bg(header, inner_w, t.bg_dark);
    }
    let mut lines = vec![header];
    if expanded {
        for member in &g.members {
            let err = member.result.as_ref().is_some_and(|r| r.is_error);
            let bullet = if err { t.accent_error } else { t.gray };
            let spans = vec![
                Span::raw("   "),
                Span::styled("◆ ", fg(bullet)),
                Span::styled(member.name.clone(), bold(t.gray)),
                Span::styled(format!("  {}", member.hint), fg(t.gray)),
            ];
            lines.push(clip_spans(spans, inner_w));
            if let Some(res) = &member.result {
                outcome_rows(t, &mut lines, res, inner_w);
            }
        }
    }
    Rendered {
        lines,
        foldable: true,
    }
}

fn render_run(
    t: &ViewTheme,
    r: &RunBlock,
    inner_w: usize,
    expanded: bool,
    selected: bool,
) -> Rendered {
    let collapsed_sel = selected && !expanded;
    let err = r.result.as_ref().is_some_and(|res| res.is_error);
    let bullet_color = if err { t.accent_error } else { t.gray };
    let bullet = if collapsed_sel { "› " } else { "◆ " };
    let (label_style, desc_style) = if collapsed_sel {
        (bold(t.text_primary), fg(t.text_primary))
    } else {
        (bold(t.gray), fg(t.gray))
    };
    let spans = vec![
        Span::raw("   "),
        Span::styled(bullet, fg(bullet_color)),
        Span::styled("Run ", label_style),
        Span::styled(r.description.clone(), desc_style),
    ];
    let mut header = clip_spans(spans, inner_w);
    if collapsed_sel {
        header = patch_bg(header, inner_w, t.bg_dark);
    }
    let mut lines = vec![header];
    if expanded {
        if !r.command.is_empty() {
            command_rows(t, &mut lines, &r.command, inner_w);
        }
        if let Some(res) = &r.result {
            outcome_rows(t, &mut lines, res, inner_w);
        }
    }
    Rendered {
        lines,
        foldable: true,
    }
}

fn render_tool(
    t: &ViewTheme,
    tool: &ToolBlock,
    inner_w: usize,
    expanded: bool,
    selected: bool,
) -> Rendered {
    let collapsed_sel = selected && !expanded;
    let err = tool.result.as_ref().is_some_and(|res| res.is_error);
    let bullet_color = if err { t.accent_error } else { t.gray };
    let bullet = if collapsed_sel { "› " } else { "◆ " };
    let name_style = if collapsed_sel {
        bold(t.text_primary)
    } else {
        bold(t.gray)
    };
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(bullet, fg(bullet_color)),
        Span::styled(tool.name.clone(), name_style),
    ];
    if !tool.hint.is_empty() {
        spans.push(Span::styled(format!("  {}", tool.hint), fg(t.gray)));
    }
    let mut header = clip_spans(spans, inner_w);
    if collapsed_sel {
        header = patch_bg(header, inner_w, t.bg_dark);
    }
    let mut lines = vec![header];
    if expanded {
        if let Some(res) = &tool.result {
            outcome_rows(t, &mut lines, res, inner_w);
        }
    }
    Rendered {
        lines,
        foldable: true,
    }
}

fn render_entry(
    t: &ViewTheme,
    block: &DisplayBlock,
    inner_w: usize,
    expanded: bool,
    selected: bool,
) -> Rendered {
    match block {
        DisplayBlock::User(u) => render_user(t, u, inner_w, expanded),
        DisplayBlock::Assistant(a) => render_assistant(t, a, inner_w),
        DisplayBlock::ToolGroup(g) => render_group(t, g, inner_w, expanded, selected),
        DisplayBlock::Run(r) => render_run(t, r, inner_w, expanded, selected),
        DisplayBlock::Tool(tool) => render_tool(t, tool, inner_w, expanded, selected),
        DisplayBlock::Thinking(tb) => render_thinking(t, tb, inner_w, expanded, selected),
        DisplayBlock::WorkedFor(w) => Rendered {
            lines: vec![clip_spans(
                vec![Span::raw("   "), Span::styled(w.label(), fg(t.gray))],
                inner_w,
            )],
            foldable: false,
        },
    }
}

fn fold_kind(block: &DisplayBlock) -> FoldKind {
    match block {
        DisplayBlock::Thinking(_) => FoldKind::Thinking,
        DisplayBlock::ToolGroup(_) | DisplayBlock::Run(_) | DisplayBlock::Tool(_) => FoldKind::Tool,
        DisplayBlock::User(_) => FoldKind::User,
        DisplayBlock::Assistant(_) | DisplayBlock::WorkedFor(_) => FoldKind::Fixed,
    }
}

/// The text `/find` matches against, per block.
fn search_text(block: &DisplayBlock) -> String {
    match block {
        DisplayBlock::User(u) => u.text.clone(),
        DisplayBlock::Assistant(a) => a.markdown.clone(),
        DisplayBlock::Thinking(tb) => format!("{} {}", tb.label(), tb.text),
        DisplayBlock::Run(r) => {
            let mut s = format!("Run {} {}", r.description, r.command);
            if let Some(res) = &r.result {
                s.push(' ');
                s.push_str(&res.text);
            }
            s
        }
        DisplayBlock::Tool(tool) => {
            let mut s = format!("{} {} {}", tool.name, tool.hint, tool.input_json);
            if let Some(res) = &tool.result {
                s.push(' ');
                s.push_str(&res.text);
            }
            s
        }
        DisplayBlock::ToolGroup(g) => {
            let mut s = g.label();
            for m in &g.members {
                s.push(' ');
                s.push_str(&m.name);
                s.push(' ');
                s.push_str(&m.hint);
                if let Some(res) = &m.result {
                    s.push(' ');
                    s.push_str(&res.text);
                }
            }
            s
        }
        DisplayBlock::WorkedFor(_) => String::new(),
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

/// grok's scrollback-focused pager scroll bindings, minus the keys the
/// interaction layer now owns (Up/Down select, q quits at the app level):
/// j/k line scroll, Ctrl+J/K line scroll, Ctrl+D/U half page, PageUp/Down
/// page, g/G top/bottom.
fn handle_scroll_key(scroll: &mut Scroll, viewport_h: usize, code: KeyCode, mods: KeyModifiers) {
    if mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('j') => scroll.scroll_down(1),
            KeyCode::Char('k') => scroll.scroll_up(1),
            KeyCode::Char('d') => scroll.scroll_down(half_page_rows(viewport_h)),
            KeyCode::Char('u') => scroll.scroll_up(half_page_rows(viewport_h)),
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Char('j') => scroll.scroll_down(1),
        KeyCode::Char('k') => scroll.scroll_up(1),
        KeyCode::Char('g') => scroll.goto_top(),
        KeyCode::Char('G') => scroll.goto_bottom(),
        KeyCode::PageDown => scroll.scroll_down(page_rows(viewport_h)),
        KeyCode::PageUp => scroll.scroll_up(page_rows(viewport_h)),
        _ => {}
    }
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
// Block viewer overlay (grok views/block_viewer.rs + modal_window.rs)
// ---------------------------------------------------------------------------

struct Viewer {
    title: String,
    block: DisplayBlock,
    scroll: usize,
    lines: Vec<Line<'static>>,
    cache_w: usize,
    /// Content rows of the last draw, for page scrolling.
    view_h: usize,
}

fn viewer_title(block: &DisplayBlock) -> String {
    match block {
        DisplayBlock::User(_) => "User".to_string(),
        DisplayBlock::Assistant(_) => "Assistant".to_string(),
        DisplayBlock::Thinking(_) => "Thinking".to_string(),
        DisplayBlock::Run(_) => "Run".to_string(),
        DisplayBlock::Tool(tool) => tool.name.clone(),
        DisplayBlock::ToolGroup(g) => g.label(),
        DisplayBlock::WorkedFor(_) => "Turn".to_string(),
    }
}

/// One viewer output row on the full-width `bg_dark` band (same style as the
/// scrollback's expanded outcome band).
fn viewer_band_row(t: &ViewTheme, text: String, text_fg: Color, width: usize) -> Line<'static> {
    let used = UnicodeWidthStr::width(text.as_str());
    let mut spans = vec![Span::styled(text, fg(text_fg).bg(t.bg_dark))];
    if width > used {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(t.bg_dark),
        ));
    }
    Line::from(spans)
}

fn viewer_outcome_lines(
    out: &mut Vec<Line<'static>>,
    t: &ViewTheme,
    width: usize,
    result: &Option<ToolOutcome>,
) {
    let Some(res) = result else {
        return;
    };
    if res.is_error {
        for src in res.text.trim_end().lines() {
            for piece in wrap_plain(src, width) {
                out.push(Line::from(Span::styled(piece, fg(t.accent_error))));
            }
        }
        if res.truncated {
            out.push(Line::from(Span::styled(
                "… output truncated".to_string(),
                fg(t.gray),
            )));
        }
        return;
    }
    for src in res.text.trim_end().lines() {
        for piece in wrap_plain(src, width) {
            out.push(viewer_band_row(t, piece, t.text_primary, width));
        }
    }
    if res.truncated {
        out.push(viewer_band_row(
            t,
            "… output truncated".to_string(),
            t.gray,
            width,
        ));
    }
}

fn viewer_lines(block: &DisplayBlock, t: &ViewTheme, width: usize) -> Vec<Line<'static>> {
    let width = width.max(4);
    let md = |text: &str| -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for line in grok_md::render_ratatui(text, t, width) {
            out.extend(wrap_line(&line, width));
        }
        out
    };
    let mut out = Vec::new();
    match block {
        DisplayBlock::User(u) => {
            for src in u.text.lines() {
                for piece in wrap_plain(src, width) {
                    out.push(Line::from(Span::styled(piece, fg(t.text_primary))));
                }
            }
        }
        DisplayBlock::Assistant(a) => out = md(&a.markdown),
        DisplayBlock::Thinking(tb) => out = md(&tb.text),
        DisplayBlock::Run(r) => {
            for (i, src) in r.command.lines().enumerate() {
                let prefix = if i == 0 { "$ " } else { "  " };
                for (j, piece) in wrap_plain(src, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if j == 0 { prefix } else { "  " };
                    out.push(Line::from(vec![
                        Span::styled(lead.to_string(), fg(t.gray_dim)),
                        Span::styled(piece, fg(t.command_fg)),
                    ]));
                }
            }
            if !out.is_empty() {
                out.push(Line::default());
            }
            viewer_outcome_lines(&mut out, t, width, &r.result);
        }
        DisplayBlock::Tool(tool) => {
            for src in tool.input_json.lines() {
                for piece in wrap_plain(src, width) {
                    out.push(Line::from(Span::styled(piece, fg(t.text_secondary))));
                }
            }
            if !out.is_empty() {
                out.push(Line::default());
            }
            viewer_outcome_lines(&mut out, t, width, &tool.result);
        }
        DisplayBlock::ToolGroup(g) => {
            for (i, member) in g.members.iter().enumerate() {
                if i > 0 {
                    out.push(Line::default());
                }
                out.push(Line::from(vec![
                    Span::styled("◆ ", fg(t.gray)),
                    Span::styled(member.name.clone(), bold(t.text_primary)),
                    Span::styled(format!("  {}", member.hint), fg(t.gray)),
                ]));
                viewer_outcome_lines(&mut out, t, width, &member.result);
            }
        }
        DisplayBlock::WorkedFor(w) => out.push(Line::from(Span::styled(w.label(), fg(t.gray)))),
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "(no content)".to_string(),
            fg(t.gray),
        )));
    }
    out
}

impl Viewer {
    fn new(block: DisplayBlock) -> Self {
        Viewer {
            title: viewer_title(&block),
            block,
            scroll: 0,
            lines: Vec::new(),
            cache_w: 0,
            view_h: 0,
        }
    }

    fn invalidate(&mut self) {
        self.cache_w = 0;
        self.lines.clear();
    }

    fn build(&mut self, t: &ViewTheme, width: usize) {
        if self.cache_w == width && !self.lines.is_empty() {
            return;
        }
        self.lines = viewer_lines(&self.block, t, width);
        self.cache_w = width;
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// A cached per-entry render keyed by the states that change its pixels.
struct CachedEntry {
    expanded: bool,
    selected: bool,
    foldable: bool,
    lines: Vec<Line<'static>>,
}

/// Where an entry landed in the assembled scrollback, plus its capabilities.
#[derive(Clone, Copy)]
struct LayoutEntry {
    id: u64,
    start: usize,
    height: usize,
    selectable: bool,
    is_turn: bool,
    foldable: bool,
    kind: FoldKind,
}

struct App {
    theme: &'static ViewTheme,
    parser: TranscriptParser,
    chrome: Chrome,
    finalized: Vec<Entry>,
    fold: FoldState,
    selected: Option<u64>,
    palette: Option<Palette>,
    viewer: Option<Viewer>,
    find_query: Option<String>,
    last_click: Option<(Instant, u64)>,
    scroll: Scroll,
    viewport_h: usize,
    scroll_rect: Rect,
    layout: Vec<LayoutEntry>,
    cache: HashMap<u64, CachedEntry>,
    cache_width: usize,
    cache_theme: ThemeKind,
}

impl App {
    fn new(theme: &'static ViewTheme) -> Self {
        App {
            theme,
            parser: TranscriptParser::new(),
            chrome: Chrome::default(),
            finalized: Vec::new(),
            fold: FoldState::new(),
            selected: None,
            palette: None,
            viewer: None,
            find_query: None,
            last_click: None,
            scroll: Scroll::new(),
            viewport_h: 0,
            scroll_rect: Rect::default(),
            layout: Vec::new(),
            cache: HashMap::new(),
            cache_width: 0,
            cache_theme: theme.kind,
        }
    }

    fn push_raw(&mut self, raw: &str) {
        if let Ok(row) = serde_json::from_str::<Value>(raw) {
            self.chrome.update(&row);
        }
        self.finalized.extend(self.parser.push_entries(raw));
    }

    /// Assemble the scrollback: cached finalized entries (re-rendered when
    /// their width/fold/selection state changes) + freshly rendered pending,
    /// recording each entry's line range for selection and hit tests.
    fn scrollback_lines(&mut self, inner_w: usize) -> Vec<Line<'static>> {
        if self.cache_width != inner_w || self.cache_theme != self.theme.kind {
            self.cache.clear();
            self.cache_width = inner_w;
            self.cache_theme = self.theme.kind;
        }
        let pending = self.parser.pending_entries();
        let n_final = self.finalized.len();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut layout: Vec<LayoutEntry> = Vec::new();
        let mut last_dense = false;
        for (i, entry) in self.finalized.iter().chain(pending.iter()).enumerate() {
            let kind = fold_kind(&entry.block);
            let expanded = self.fold.expanded(entry.id, kind);
            let selected = self.selected == Some(entry.id);
            let dense = matches!(
                entry.block,
                DisplayBlock::ToolGroup(_)
                    | DisplayBlock::Run(_)
                    | DisplayBlock::Tool(_)
                    | DisplayBlock::Thinking(_)
            );
            if !lines.is_empty() && !(dense && last_dense) {
                lines.push(Line::default());
            }
            last_dense = dense;
            let (entry_lines, foldable) = if i < n_final {
                match self.cache.get(&entry.id) {
                    Some(c) if c.expanded == expanded && c.selected == selected => {
                        (c.lines.clone(), c.foldable)
                    }
                    _ => {
                        let r = render_entry(self.theme, &entry.block, inner_w, expanded, selected);
                        self.cache.insert(
                            entry.id,
                            CachedEntry {
                                expanded,
                                selected,
                                foldable: r.foldable,
                                lines: r.lines.clone(),
                            },
                        );
                        (r.lines, r.foldable)
                    }
                }
            } else {
                let r = render_entry(self.theme, &entry.block, inner_w, expanded, selected);
                (r.lines, r.foldable)
            };
            layout.push(LayoutEntry {
                id: entry.id,
                start: lines.len(),
                height: entry_lines.len(),
                selectable: !matches!(entry.block, DisplayBlock::WorkedFor(_)),
                is_turn: entry.block.starts_turn(),
                foldable,
                kind,
            });
            lines.extend(entry_lines);
        }
        self.layout = layout;
        lines
    }

    fn infos(&self) -> Vec<EntryInfo> {
        self.layout
            .iter()
            .map(|le| EntryInfo {
                id: le.id,
                selectable: le.selectable,
                is_turn: le.is_turn,
            })
            .collect()
    }

    fn layout_of(&self, id: u64) -> Option<LayoutEntry> {
        self.layout.iter().copied().find(|le| le.id == id)
    }

    fn scroll_to_selected(&mut self) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if let Some(offset) =
            interact::scroll_into_view(self.scroll.offset, self.viewport_h, le.start, le.height)
        {
            self.scroll.offset = offset.min(self.scroll.max);
            self.scroll.follow = false;
        }
    }

    fn move_selection(&mut self, forward: bool) {
        let infos = self.infos();
        let mv = if forward {
            interact::select_next(&infos, self.selected)
        } else {
            interact::select_prev(&infos, self.selected)
        };
        match mv {
            SelectMove::To(id) => {
                self.selected = Some(id);
                self.scroll_to_selected();
            }
            SelectMove::Overscroll => self.scroll.goto_bottom(),
            SelectMove::Stay => {}
        }
    }

    /// Shift+Left/Right: select the turn's prompt and snap it to the top.
    fn jump_turn(&mut self, forward: bool) {
        let infos = self.infos();
        let target = if forward {
            interact::next_turn(&infos, self.selected)
        } else {
            interact::prev_turn(&infos, self.selected)
        };
        let Some(id) = target else { return };
        self.selected = Some(id);
        if let Some(le) = self.layout_of(id) {
            self.scroll.offset = le.start.min(self.scroll.max);
            self.scroll.follow = false;
        }
    }

    fn fold_selected(&mut self, expanded: bool) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if le.foldable {
            self.fold.set(le.id, le.kind, expanded);
            self.scroll_to_selected();
        }
    }

    fn toggle_selected_fold(&mut self) {
        let Some(le) = self.selected.and_then(|id| self.layout_of(id)) else {
            return;
        };
        if le.foldable {
            self.fold.toggle(le.id, le.kind);
        }
    }

    fn toggle_all_thinking(&mut self) {
        let ids: Vec<u64> = self
            .layout
            .iter()
            .filter(|le| le.kind == FoldKind::Thinking)
            .map(|le| le.id)
            .collect();
        self.fold.toggle_all_thinking(&ids);
    }

    fn block_of(&self, id: u64) -> Option<DisplayBlock> {
        self.finalized
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.block.clone())
            .or_else(|| {
                self.parser
                    .pending_entries()
                    .into_iter()
                    .find(|e| e.id == id)
                    .map(|e| e.block)
            })
    }

    fn open_viewer(&mut self) {
        let Some(block) = self.selected.and_then(|id| self.block_of(id)) else {
            return;
        };
        self.viewer = Some(Viewer::new(block));
    }

    /// `/theme`: switch live, then persist through the settings store the
    /// startup resolution reads (`view.theme`). `auto` re-detects from the
    /// env stamps only — the OSC 11 probe needs the raw tty, which crossterm
    /// owns mid-session.
    fn apply_theme(&mut self, pref: Option<ThemePref>) {
        use crate::view_theme::{parse_appearance_var, parse_colorfgbg, resolve_kind};
        let (kind, persist) = match pref {
            None => match self.theme.kind {
                ThemeKind::Dark => (ThemeKind::Light, "light"),
                ThemeKind::Light => (ThemeKind::Dark, "dark"),
            },
            Some(ThemePref::Light) => (ThemeKind::Light, "light"),
            Some(ThemePref::Dark) => (ThemeKind::Dark, "dark"),
            Some(ThemePref::Auto) => {
                let detected =
                    parse_appearance_var(std::env::var("HIVE_APPEARANCE").ok().as_deref())
                        .or_else(|| parse_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()));
                (resolve_kind(ThemePref::Auto, detected), "auto")
            }
        };
        self.theme = kind.theme();
        if let Some(v) = &mut self.viewer {
            v.invalidate();
        }
        let _ = crate::settings::set_setting("view.theme", serde_json::json!(persist));
    }

    fn run_find(&mut self, forward: bool) {
        let Some(query) = self.find_query.clone() else {
            return;
        };
        let pending = self.parser.pending_entries();
        let list: Vec<(u64, String)> = self
            .finalized
            .iter()
            .chain(pending.iter())
            .filter(|e| !matches!(e.block, DisplayBlock::WorkedFor(_)))
            .map(|e| (e.id, search_text(&e.block)))
            .collect();
        if let Some(id) = interact::find_match(&list, self.selected, &query, forward) {
            self.selected = Some(id);
            self.scroll_to_selected();
        }
    }

    // ---- key/mouse routing ---------------------------------------------

    /// Returns true when the app should quit.
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            return true;
        }
        if self.viewer.is_some() {
            self.viewer_key(code, mods);
            return false;
        }
        if self.palette.is_some() {
            return self.palette_key(code, mods);
        }
        self.main_key(code, mods)
    }

    fn main_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('f') => self.open_viewer(),
                KeyCode::Char('e') => self.toggle_all_thinking(),
                KeyCode::Char('o') => self.fold.cycle_density(),
                _ => handle_scroll_key(&mut self.scroll, self.viewport_h, code, mods),
            }
            return false;
        }
        if mods.contains(KeyModifiers::SHIFT) && matches!(code, KeyCode::Left | KeyCode::Right) {
            self.jump_turn(code == KeyCode::Right);
            return false;
        }
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('/') => self.palette = Some(Palette::open()),
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down => self.move_selection(true),
            KeyCode::Left => self.fold_selected(false),
            KeyCode::Right => self.fold_selected(true),
            KeyCode::Enter => self.open_viewer(),
            KeyCode::Char('n') => self.run_find(true),
            KeyCode::Char('N') => self.run_find(false),
            _ => handle_scroll_key(&mut self.scroll, self.viewport_h, code, mods),
        }
        false
    }

    fn viewer_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let closes = matches!(code, KeyCode::Esc)
            || (!ctrl && code == KeyCode::Char('q'))
            || (ctrl && code == KeyCode::Char('f'));
        if closes {
            self.viewer = None;
            return;
        }
        let Some(v) = &mut self.viewer else { return };
        let page = page_rows(v.view_h);
        let half = half_page_rows(v.view_h);
        if ctrl {
            match code {
                KeyCode::Char('j') => v.scroll += 1,
                KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
                KeyCode::Char('d') => v.scroll += half,
                KeyCode::Char('u') => v.scroll = v.scroll.saturating_sub(half),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => v.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageDown => v.scroll += page,
            KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(page),
            KeyCode::Char('g') => v.scroll = 0,
            KeyCode::Char('G') => v.scroll = usize::MAX / 2,
            _ => {}
        }
    }

    fn palette_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter => {
                let action = self.palette.as_ref().map(Palette::enter);
                match action {
                    Some(PaletteAction::SwitchTheme(pref)) => {
                        self.palette = None;
                        self.apply_theme(pref);
                    }
                    Some(PaletteAction::SetDensity(density)) => {
                        self.palette = None;
                        self.fold.set_density(density);
                    }
                    Some(PaletteAction::Find(query)) => {
                        self.palette = None;
                        self.find_query = Some(query);
                        self.run_find(true);
                    }
                    Some(PaletteAction::Quit) => return true,
                    Some(PaletteAction::Complete(name)) => {
                        if let Some(p) = &mut self.palette {
                            p.input = format!("{name} ");
                        }
                    }
                    Some(PaletteAction::Noop) | None => {}
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = &mut self.palette {
                    if !p.backspace() {
                        self.palette = None;
                    }
                }
            }
            KeyCode::Up => {
                if let Some(p) = &mut self.palette {
                    p.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(p) = &mut self.palette {
                    p.move_down();
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                if let Some(p) = &mut self.palette {
                    p.insert(c);
                }
            }
            _ => {}
        }
        false
    }

    fn on_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16, now: Instant) {
        match kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                if let Some(v) = &mut self.viewer {
                    if kind == MouseEventKind::ScrollDown {
                        v.scroll += WHEEL_LINES;
                    } else {
                        v.scroll = v.scroll.saturating_sub(WHEEL_LINES);
                    }
                } else {
                    handle_mouse(&mut self.scroll, kind);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => self.on_click(x, y, now),
            _ => {}
        }
    }

    /// Single click selects the entry under the cursor; a second click on the
    /// same entry within the multi-click window toggles its fold.
    fn on_click(&mut self, x: u16, y: u16, now: Instant) {
        if self.viewer.is_some() || self.palette.is_some() {
            return;
        }
        let r = self.scroll_rect;
        if x < r.x || x >= r.x + r.width || y < r.y || y >= r.y + r.height {
            return;
        }
        let line_idx = self.scroll.offset + (y - r.y) as usize;
        let Some(le) =
            self.layout.iter().copied().find(|le| {
                le.selectable && line_idx >= le.start && line_idx < le.start + le.height
            })
        else {
            return;
        };
        let double = self
            .last_click
            .is_some_and(|(t0, id)| id == le.id && now.duration_since(t0) <= MULTI_CLICK);
        self.selected = Some(le.id);
        if double {
            self.toggle_selected_fold();
            self.last_click = None;
        } else {
            self.last_click = Some((now, le.id));
        }
    }
}

// ---------------------------------------------------------------------------
// Frame chrome
// ---------------------------------------------------------------------------

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

fn bottom_line(t: &ViewTheme, _model: Option<&str>, inner_w: usize) -> Line<'static> {
    // grok hint row: left-aligned "Key:label" pairs with │ separators
    // (Shift+Tab:mode │ Ctrl+x:shortcuts); model/effort live on the
    // composer's bottom border instead.
    let key = bold(t.text_secondary);
    let label = fg(t.gray);
    let sep = fg(t.gray).add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (i, (k, what)) in [
        ("↑↓", "select"),
        ("←→", "fold"),
        ("/", "cmd"),
        ("q", "quit"),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            spans.push(Span::styled("  │  ", sep));
        }
        spans.push(Span::styled(k.to_string(), key));
        spans.push(Span::styled(format!(":{what}"), label));
    }
    let line = Line::from(spans);
    if cells_width(&line_cells(&line)) > inner_w {
        return clip_spans(line.spans, inner_w);
    }
    line
}

/// grok SelectionBox: fg-only `┌ ┐ └ ┘` corners one row outside the entry,
/// `│` sides in the padding columns, dashed `┆` where the viewport clips.
fn draw_selection_frame(
    buf: &mut Buffer,
    t: &ViewTheme,
    rect: Rect,
    offset: usize,
    le: LayoutEntry,
) {
    let h = rect.height as usize;
    if h == 0 || rect.width < 8 {
        return;
    }
    let start = le.start;
    let end = le.start + le.height;
    let vis_top = start.max(offset);
    let vis_end = end.min(offset + h);
    if vis_top >= vis_end {
        return;
    }
    let top_clipped = start < offset;
    let bottom_clipped = end > offset + h;
    let left = rect.x + 2;
    let right = rect.x + rect.width - 2;
    let style = Style::default().fg(t.selection_border);
    for row in vis_top..vis_end {
        let y = rect.y + (row - offset) as u16;
        let sym = if (row == vis_top && top_clipped) || (row == vis_end - 1 && bottom_clipped) {
            "┆"
        } else {
            "│"
        };
        for x in [left, right] {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }
    if !top_clipped && start > offset {
        let y = rect.y + (start - offset) as u16 - 1;
        for (x, sym) in [(left, "┌"), (right, "┐")] {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }
    if !bottom_clipped && end < offset + h {
        let y = rect.y + (end - offset) as u16;
        for (x, sym) in [(left, "└"), (right, "┘")] {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }
}

/// Composer box height: top border, one input row, bottom border.
const COMPOSER_H: u16 = 3;

/// grok's composer box (prompt_widget/mod.rs draw): rounded `╭─╮` top and
/// `╰─╯` bottom dividers with `│` sides on prompt_border (active shade while
/// the palette is typing into it), content inset 2 cols, `❯ ` prefix —
/// accent_user when focused, gray_dim idle (grok PromptStyle::accent_color).
/// Permanently read-only: the slash palette is the only thing that ever
/// types here; idle it shows just the prompt arrow, hints stay on the row
/// below the box (grok keeps its shortcuts bar under the composer).
fn draw_composer(frame: &mut Frame, app: &App, rect: Rect) {
    let t = app.theme;
    if rect.width < 4 || rect.height < COMPOSER_H {
        return;
    }
    let focused = app.palette.is_some();
    let border = fg(if focused {
        t.prompt_border_active
    } else {
        t.prompt_border
    });
    let w = rect.width as usize;
    let top = Line::from(Span::styled(format!("╭{}╮", "─".repeat(w - 2)), border));
    let bottom = {
        // grok embeds the model badge in the bottom border, right-aligned:
        // "─ Grok 4.6 (xhigh) · always-approve ─" — ours carries model,
        // effort, and the read-only marker.
        let mut badge: Vec<Span<'static>> = Vec::new();
        if let Some(m) = app.parser.model() {
            badge.push(Span::styled(format!(" {m}"), fg(t.accent_model)));
            if let Some(e) = app.parser.effort() {
                badge.push(Span::styled(format!(" ({e})"), fg(t.gray)));
            }
            badge.push(Span::styled(
                " · ".to_string(),
                fg(t.gray).add_modifier(Modifier::DIM),
            ));
        }
        badge.push(Span::styled("read-only ".to_string(), fg(t.gray)));
        let badge_w = cells_width(&line_cells(&Line::from(badge.clone())));
        if w >= badge_w + 6 {
            let left = w - 3 - badge_w;
            let mut spans = vec![Span::styled(format!("╰{}", "─".repeat(left)), border)];
            spans.extend(badge);
            spans.push(Span::styled("─╯".to_string(), border));
            Line::from(spans)
        } else {
            Line::from(Span::styled(format!("╰{}╯", "─".repeat(w - 2)), border))
        }
    };
    let prefix_color = if focused { t.accent_user } else { t.gray_dim };
    let mut spans = vec![
        Span::styled("│".to_string(), border),
        Span::raw(" "),
        Span::styled("❯ ".to_string(), fg(prefix_color)),
    ];
    if let Some(pal) = &app.palette {
        spans.push(Span::styled(pal.input.clone(), fg(t.text_primary)));
    } else {
        spans.push(Span::styled(
            "read-only".to_string(),
            fg(t.gray_dim).add_modifier(Modifier::DIM),
        ));
    }
    let mut mid = clip_spans(spans, w - 1);
    let used = cells_width(&line_cells(&mid));
    if w > used + 1 {
        mid.spans.push(Span::raw(" ".repeat(w - 1 - used)));
    }
    mid.spans.push(Span::styled("│".to_string(), border));
    let base = Style::default().bg(t.bg_base).fg(t.text_primary);
    frame.render_widget(Paragraph::new(vec![top, mid, bottom]).style(base), rect);
}

/// grok slash palette: the input types into the composer box; the dropdown
/// panel anchors directly above `anchor_y` (the box top; in the tiny-pane
/// fallback the hint row, where `input_rect` renders the input instead) —
/// bg_light rows, bg_visual+bold selected row with a `❯ ` prefix, gray
/// descriptions in an aligned column, fuzzy-match chars in fuzzy_accent,
/// `─` rules top and bottom.
fn draw_palette(
    frame: &mut Frame,
    app: &App,
    inner: Rect,
    anchor_y: u16,
    input_rect: Option<Rect>,
) {
    let t = app.theme;
    let Some(pal) = &app.palette else { return };
    let base = Style::default().bg(t.bg_base).fg(t.text_primary);
    let w = inner.width as usize;
    if let Some(rect) = input_rect {
        let input_spans = vec![
            Span::styled("❯ ".to_string(), bold(t.accent_user)),
            Span::styled(pal.input.clone(), fg(t.text_primary)),
        ];
        frame.render_widget(Paragraph::new(clip_spans(input_spans, w)).style(base), rect);
    }
    let hits = pal.filtered();
    if hits.is_empty() {
        return;
    }
    let avail = anchor_y.saturating_sub(inner.y) as usize;
    let rows = hits
        .len()
        .min(MAX_PALETTE_ROWS)
        .min(avail.saturating_sub(2));
    if rows == 0 {
        return;
    }
    let panel_h = rows + 2;
    let panel = Rect {
        x: inner.x,
        y: anchor_y - panel_h as u16,
        width: inner.width,
        height: panel_h as u16,
    };
    let selected_row = pal.selected_row();
    let label_w = hits
        .iter()
        .map(|&(i, _)| PALETTE_COMMANDS[i].name.len())
        .max()
        .unwrap_or(0)
        .min((w * 6 / 10).min(40));
    let rule = |count: Option<String>| -> Line<'static> {
        let rule_style = fg(t.bg_light);
        match count {
            Some(text) if w > text.len() + 2 => Line::from(vec![
                Span::styled("─".repeat(w - text.len() - 1), rule_style),
                Span::styled(text, fg(t.gray)),
                Span::styled("─".to_string(), rule_style),
            ]),
            _ => Line::from(Span::styled("─".repeat(w), rule_style)),
        }
    };
    let count_text = format!(
        " {} match{} ",
        hits.len(),
        if hits.len() == 1 { "" } else { "es" }
    );
    let mut lines: Vec<Line<'static>> = vec![rule(Some(count_text))];
    for (row, &(cmd_idx, ref positions)) in hits.iter().take(rows).enumerate() {
        let cmd = &PALETTE_COMMANDS[cmd_idx];
        let is_sel = row == selected_row;
        let row_bg = if is_sel { t.bg_visual } else { t.bg_light };
        let mut cells: Vec<Cell> = Vec::new();
        let pad_style = Style::default().bg(row_bg);
        for _ in 0..2 {
            cells.push((' ', pad_style));
        }
        let prefix = if is_sel { "❯ " } else { "  " };
        for ch in prefix.chars() {
            cells.push((ch, fg(t.text_primary).bg(row_bg)));
        }
        for (ci, ch) in cmd.name.chars().enumerate() {
            let mut st = if positions.contains(&ci) {
                fg(t.fuzzy_accent).bg(row_bg)
            } else {
                fg(t.text_primary).bg(row_bg)
            };
            if is_sel {
                st = st.add_modifier(Modifier::BOLD);
            }
            cells.push((ch, st));
        }
        while cells.len() < 4 + label_w {
            cells.push((' ', pad_style));
        }
        for _ in 0..2 {
            cells.push((' ', pad_style));
        }
        let desc_budget = w.saturating_sub(cells_width(&cells) + 1);
        for ch in clip_plain(cmd.desc, desc_budget).chars() {
            cells.push((ch, fg(t.gray).bg(row_bg)));
        }
        while cells_width(&cells) < w {
            cells.push((' ', pad_style));
        }
        lines.push(cells_to_line(&cells));
    }
    lines.push(rule(None));
    frame.render_widget(Clear, panel);
    frame.render_widget(Paragraph::new(lines).style(base), panel);
}

/// grok modal window: centered popup (90% width clamped to [60,140], 7-row
/// vertical margins), square gray_dim border, `─ Title ─` on the top border,
/// bg_base fill, scrollable content, footer hints.
fn draw_viewer(frame: &mut Frame, app: &mut App) {
    let t = app.theme;
    let Some(v) = &mut app.viewer else { return };
    let area = frame.area();
    if area.width < 20 || area.height < 8 {
        return;
    }
    let w = ((area.width as usize * 9 / 10).clamp(60, 140) as u16).min(area.width);
    let (y, h) = if area.height > 20 {
        (area.y + 7, area.height - 14)
    } else {
        (area.y + 1, area.height - 2)
    };
    let popup = Rect {
        x: area.x + (area.width - w) / 2,
        y,
        width: w,
        height: h,
    };
    let border_style = fg(t.gray_dim).bg(t.bg_base);
    let title = Line::from(vec![
        Span::styled("─ ".to_string(), border_style),
        Span::styled(v.title.clone(), bold(t.text_primary).bg(t.bg_base)),
        Span::styled(" ─".to_string(), border_style),
    ]);
    let block = Block::bordered()
        .border_style(border_style)
        .title(title)
        .style(Style::default().bg(t.bg_base));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    if inner.width < 6 || inner.height < 3 {
        return;
    }
    let content = Rect {
        x: inner.x + 2,
        y: inner.y + 1,
        width: inner.width.saturating_sub(4),
        height: inner.height.saturating_sub(3),
    };
    v.build(t, content.width as usize);
    v.view_h = content.height as usize;
    let max = v.lines.len().saturating_sub(content.height as usize);
    v.scroll = v.scroll.min(max);
    let end = (v.scroll + content.height as usize).min(v.lines.len());
    let visible: Vec<Line> = v.lines[v.scroll..end].to_vec();
    let base = Style::default().bg(t.bg_base).fg(t.text_primary);
    frame.render_widget(Paragraph::new(visible).style(base), content);
    let footer = Rect {
        x: content.x,
        y: inner.y + inner.height - 1,
        width: content.width,
        height: 1,
    };
    let hint = Line::from(Span::styled(
        "Esc close · j/k scroll · g/G ends".to_string(),
        fg(t.gray),
    ));
    frame.render_widget(Paragraph::new(hint).style(base), footer);
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
    // The composer box sits between the scrollback and the hint row; a pane
    // too short for status + gap + one scroll row + box + hint drops it.
    let have_box = inner.height >= 2 + gap + COMPOSER_H + 1;
    let box_rect = have_box.then(|| Rect {
        x: inner.x,
        y: hint_rect.y - COMPOSER_H,
        width: inner.width,
        height: COMPOSER_H,
    });
    let reserved = 2 + gap + if have_box { COMPOSER_H } else { 0 };
    let scroll_h = inner.height.saturating_sub(reserved);
    let scroll_rect = Rect {
        x: inner.x,
        y: inner.y + 1 + gap,
        width: inner.width,
        height: scroll_h,
    };

    let base = Style::default().bg(t.bg_base).fg(t.text_primary);
    let lines = app.scrollback_lines(inner.width as usize);
    app.viewport_h = scroll_h as usize;
    app.scroll_rect = scroll_rect;
    app.scroll
        .sync(lines.len().saturating_sub(scroll_h as usize));
    let end = (app.scroll.offset + scroll_h as usize).min(lines.len());
    let visible: Vec<Line> = lines[app.scroll.offset..end].to_vec();
    frame.render_widget(Paragraph::new(visible).style(base), scroll_rect);
    if let Some(le) = app.selected.and_then(|id| app.layout_of(id)) {
        draw_selection_frame(frame.buffer_mut(), t, scroll_rect, app.scroll.offset, le);
    }
    frame.render_widget(
        Paragraph::new(top_line(app, inner.width as usize)).style(base),
        status_rect,
    );
    if let Some(bx) = box_rect {
        // Palette input renders inside the box; the hint row stays below it.
        draw_composer(frame, app, bx);
        frame.render_widget(
            Paragraph::new(bottom_line(t, app.parser.model(), inner.width as usize)).style(base),
            hint_rect,
        );
        if app.palette.is_some() {
            draw_palette(frame, app, inner, bx.y, None);
        }
    } else if app.palette.is_some() {
        // Tiny-pane fallback: input at the hint row, panel above it.
        draw_palette(frame, app, inner, hint_rect.y, Some(hint_rect));
    } else {
        frame.render_widget(
            Paragraph::new(bottom_line(t, app.parser.model(), inner.width as usize)).style(base),
            hint_rect,
        );
    }
    if app.viewer.is_some() {
        draw_viewer(frame, app);
    }
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
                    if app.on_key(k.code, k.modifiers) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => app.on_mouse(m.kind, m.column, m.row, Instant::now()),
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
    use super::interact::Density;
    use super::*;
    use crate::view_theme::{GROKDAY, GROKNIGHT};
    use ratatui::backend::TestBackend;
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

    fn thinking_row(text: &str) -> String {
        json!({
            "type": "assistant", "timestamp": "2026-08-30T12:40:14.300Z",
            "message": {"content": [{"type": "thinking", "thinking": text}]},
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

    fn tool_result_row(id: &str, content: &str, is_error: bool) -> String {
        json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": id,
                 "content": content, "is_error": is_error}
            ]},
        })
        .to_string()
    }

    fn key(app: &mut App, code: KeyCode) -> bool {
        app.on_key(code, KeyModifiers::NONE)
    }

    fn ctrl(app: &mut App, c: char) -> bool {
        app.on_key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            key(app, KeyCode::Char(c));
        }
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
        app.push_raw(&thinking_row("hmm"));
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("◈ Read 1 file, Searched 1 pattern"), "{text}");
        assert!(text.contains("◆ Run List files"), "{text}");
        assert!(text.contains("◆ Thought for 14.3s"), "{text}");
    }

    #[test]
    fn test_bottom_line_is_left_aligned_key_hints() {
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&assistant_text_row("hi"));
        let buf = draw_to_buffer(&mut app, W, H);
        let bottom = row_text(&buf, H - 2);
        let trimmed = bottom.trim();
        // grok layout: left-aligned Key:label pairs, │ separators, no model
        // (the badge moved onto the composer border), no Enter entry.
        assert_eq!(
            trimmed, "↑↓:select  │  ←→:fold  │  /:cmd  │  q:quit",
            "{bottom:?}"
        );
        assert!(!bottom.contains("Enter"), "{bottom:?}");
        // Left-aligned inside the 2-col inset + 1-space lead.
        let lead = bottom.len() - bottom.trim_start().len();
        assert!(lead <= 4, "{bottom:?}");
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
        let mut app = App::new(&GROKNIGHT);
        assert!(key(&mut app, KeyCode::Char('q')));
        assert!(ctrl(&mut app, 'q'));
        assert!(ctrl(&mut app, 'c'));
        assert!(!key(&mut app, KeyCode::Char('x')));
    }

    #[test]
    fn test_handle_key_line_page_and_jump_bindings() {
        let mut s = scroll_at(20, 50, false);
        handle_scroll_key(&mut s, 30, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(s.offset, 21);
        handle_scroll_key(&mut s, 30, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(s.offset, 20);
        // Ctrl+J / Ctrl+K single-line scroll.
        handle_scroll_key(&mut s, 30, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 21);
        handle_scroll_key(&mut s, 30, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 20);
        // Ctrl+D / Ctrl+U half page (viewport 30 → 15).
        handle_scroll_key(&mut s, 30, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 35);
        handle_scroll_key(&mut s, 30, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(s.offset, 20);
        // PageDown / PageUp (viewport 30 → 28).
        handle_scroll_key(&mut s, 30, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(s.offset, 48);
        handle_scroll_key(&mut s, 30, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(s.offset, 20);
        // g / G jumps (G also re-engages follow).
        handle_scroll_key(&mut s, 30, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(s.offset, 0);
        assert!(!s.follow);
        handle_scroll_key(&mut s, 30, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(s.offset, 50);
        assert!(s.follow);
    }

    #[test]
    fn test_j_at_bottom_reengages_follow() {
        // grok selection.rs: a single j on the last entry enters follow.
        let mut s = scroll_at(50, 50, false);
        handle_scroll_key(&mut s, 30, KeyCode::Char('j'), KeyModifiers::NONE);
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

    // ---- selection ------------------------------------------------------

    #[test]
    fn test_up_selects_and_draws_grok_bracket_frame() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&assistant_text_row("done"));
        let _ = draw_to_buffer(&mut app, W, H);
        assert_eq!(app.selected, None, "no selection until first Up/Down");
        key(&mut app, KeyCode::Up); // engage at the tail: the assistant block
        let buf = draw_to_buffer(&mut app, W, H);
        let le = app.layout_of(app.selected.unwrap()).unwrap();
        // border columns: inner.x + 2 and inner.x + inner_w - 2
        let (left, right) = (4u16, 2 + (W - 4) - 2);
        let top_y = app.scroll_rect.y + (le.start - app.scroll.offset) as u16;
        let above = top_y - 1;
        let below = top_y + le.height as u16;
        for (x, y, sym) in [
            (left, above, "┌"),
            (right, above, "┐"),
            (left, top_y, "│"),
            (right, top_y, "│"),
            (left, below, "└"),
            (right, below, "┘"),
        ] {
            let cell = buf.cell((x, y)).unwrap();
            assert_eq!(cell.symbol(), sym, "at ({x},{y})");
            assert_eq!(cell.style().fg, Some(GROKNIGHT.selection_border));
        }
        // Up again walks to the previous selectable entry (the user band).
        key(&mut app, KeyCode::Up);
        let user_le = app.layout_of(app.selected.unwrap()).unwrap();
        assert!(user_le.is_turn);
        // Down walks back; Down past the tail re-engages follow.
        key(&mut app, KeyCode::Down);
        assert_eq!(app.layout_of(app.selected.unwrap()).unwrap().id, le.id);
        app.scroll.follow = false;
        key(&mut app, KeyCode::Down);
        assert!(app.scroll.follow, "overscroll past the last entry follows");
    }

    #[test]
    fn test_shift_arrows_jump_turns() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("first"));
        app.push_raw(&assistant_text_row("a1"));
        app.push_raw(&user_row("second"));
        app.push_raw(&assistant_text_row("a2"));
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up); // last assistant
        app.on_key(KeyCode::Left, KeyModifiers::SHIFT); // current turn's prompt
        let le = app.layout_of(app.selected.unwrap()).unwrap();
        assert!(le.is_turn);
        assert_eq!(app.scroll.offset, le.start.min(app.scroll.max));
        app.on_key(KeyCode::Left, KeyModifiers::SHIFT); // previous prompt
        let first = app.layout_of(app.selected.unwrap()).unwrap();
        assert!(first.is_turn && first.id < le.id);
        app.on_key(KeyCode::Right, KeyModifiers::SHIFT); // back to the next
        assert_eq!(app.selected, Some(le.id));
    }

    // ---- fold / density -------------------------------------------------

    fn thinking_app() -> App {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&thinking_row("deep reasoning body text"));
        app.push_raw(&assistant_text_row("done"));
        app
    }

    #[test]
    fn test_right_expands_selected_thinking_block() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up); // assistant
        key(&mut app, KeyCode::Up); // thinking
        let buf = draw_to_buffer(&mut app, W, H);
        assert!(!buffer_text(&buf).contains("deep reasoning body text"));
        key(&mut app, KeyCode::Right);
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("deep reasoning body text"), "{text}");
        // expanded body rows carry the thinking accent gutter
        let body_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("deep reasoning"))
            .unwrap();
        assert_eq!(buf.cell((2, body_row)).unwrap().symbol(), "│");
        assert_eq!(
            buf.cell((2, body_row)).unwrap().style().fg,
            Some(GROKNIGHT.accent_thinking)
        );
        key(&mut app, KeyCode::Left);
        let buf = draw_to_buffer(&mut app, W, H);
        assert!(!buffer_text(&buf).contains("deep reasoning body text"));
    }

    #[test]
    fn test_selected_collapsed_thinking_header_undims_with_patch() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up); // thinking selected, collapsed
        let buf = draw_to_buffer(&mut app, W, H);
        let row = (0..H)
            .find(|&y| row_text(&buf, y).contains("Thought"))
            .unwrap();
        // bullet swapped for the expandable indicator ›
        assert!(row_text(&buf, row).contains('›'));
        let label_x = (0..W)
            .find(|&x| buf.cell((x, row)).is_some_and(|c| c.symbol() == "T"))
            .unwrap();
        let cell = buf.cell((label_x, row)).unwrap();
        assert_eq!(cell.style().fg, Some(GROKNIGHT.text_primary), "undimmed");
        assert_eq!(cell.style().bg, Some(GROKNIGHT.bg_dark), "header patch");
    }

    #[test]
    fn test_ctrl_e_toggles_all_thinking() {
        let mut app = thinking_app();
        app.push_raw(&thinking_row("second thought body"));
        app.push_raw(&assistant_text_row("done again"));
        let _ = draw_to_buffer(&mut app, W, H);
        ctrl(&mut app, 'e');
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("deep reasoning body text"), "{text}");
        assert!(text.contains("second thought body"), "{text}");
        ctrl(&mut app, 'e');
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(!text.contains("deep reasoning body text"));
        assert!(!text.contains("second thought body"));
    }

    #[test]
    fn test_ctrl_o_density_cycle_expands_thinking_then_tools() {
        let mut app = thinking_app();
        app.push_raw(&tool_use_row(
            "Bash",
            "t1",
            json!({"command": "cargo build", "description": "Build"}),
        ));
        app.push_raw(&tool_result_row("t1", "Compiling hive v0.1", false));
        app.push_raw(&assistant_text_row("built"));
        let _ = draw_to_buffer(&mut app, W, H);
        assert_eq!(app.fold.density, Density::Normal);
        ctrl(&mut app, 'o'); // thinking
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("deep reasoning body text"), "{text}");
        assert!(!text.contains("Compiling hive"), "{text}");
        ctrl(&mut app, 'o'); // verbose
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("deep reasoning body text"), "{text}");
        assert!(text.contains("Compiling hive"), "{text}");
        // verbose also reveals the run's `$ command` line
        assert!(text.contains("$ cargo build"), "{text}");
        // output rows sit on the full-width bg_dark band in primary text
        let out_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("Compiling hive"))
            .unwrap();
        let body_x = (0..W)
            .find(|&x| buf.cell((x, out_row)).unwrap().symbol() == "C")
            .unwrap();
        let body = buf.cell((body_x, out_row)).unwrap();
        assert_eq!(body.style().bg, Some(GROKNIGHT.bg_dark));
        assert_eq!(body.style().fg, Some(GROKNIGHT.text_primary));
        // the band fill runs to the content right edge
        assert_eq!(
            buf.cell((W - 5, out_row)).unwrap().style().bg,
            Some(GROKNIGHT.bg_dark)
        );
        ctrl(&mut app, 'o'); // back to normal
        assert_eq!(app.fold.density, Density::Normal);
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(!text.contains("Compiling hive"));
    }

    #[test]
    fn test_double_click_toggles_fold_and_click_selects() {
        let mut app = thinking_app();
        let buf = draw_to_buffer(&mut app, W, H);
        let row = (0..H)
            .find(|&y| row_text(&buf, y).contains("Thought"))
            .unwrap();
        let now = Instant::now();
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), 10, row, now);
        let id = app.selected.expect("click selects");
        assert_eq!(app.layout_of(id).unwrap().kind, FoldKind::Thinking);
        app.on_mouse(
            MouseEventKind::Down(MouseButton::Left),
            10,
            row,
            now + Duration::from_millis(100),
        );
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("deep reasoning body text"), "{text}");
        // two slow clicks do NOT toggle back
        let later = now + Duration::from_secs(2);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), 10, row, later);
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("deep reasoning body text"), "{text}");
    }

    // ---- viewer ---------------------------------------------------------

    #[test]
    fn test_enter_opens_viewer_and_q_closes_it_not_the_app() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up); // assistant
        key(&mut app, KeyCode::Enter);
        assert!(app.viewer.is_some());
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("─ Assistant ─"), "{text}");
        assert!(text.contains("Esc close"), "{text}");
        // the popup clears the transcript beneath it
        assert!(text.contains("done"), "viewer shows the block body: {text}");
        assert!(!key(&mut app, KeyCode::Char('q')), "q closes, not quits");
        assert!(app.viewer.is_none());
        assert!(key(&mut app, KeyCode::Char('q')), "next q quits the app");
    }

    #[test]
    fn test_ctrl_f_opens_viewer_for_run_with_command_and_output() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&tool_use_row(
            "Bash",
            "t1",
            json!({"command": "cargo nextest run\n--all", "description": "Test"}),
        ));
        app.push_raw(&tool_result_row("t1", "849 tests passed", false));
        app.push_raw(&assistant_text_row("green"));
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up); // assistant
        key(&mut app, KeyCode::Up); // run block
        ctrl(&mut app, 'f');
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("─ Run ─"), "{text}");
        assert!(text.contains("$ cargo nextest run"), "{text}");
        assert!(text.contains("849 tests passed"), "{text}");
        ctrl(&mut app, 'f');
        assert!(app.viewer.is_none(), "Ctrl+F toggles the viewer closed");
    }

    // ---- palette --------------------------------------------------------

    #[test]
    fn test_slash_opens_palette_with_rows_and_esc_closes() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Char('/'));
        let buf = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&buf);
        assert!(text.contains("❯ /"), "{text}");
        assert!(text.contains("/theme"), "{text}");
        assert!(text.contains("/view"), "{text}");
        assert!(text.contains("/find"), "{text}");
        assert!(text.contains("/quit"), "{text}");
        assert!(text.contains("4 matches"), "{text}");
        // selected row carries the bg_visual band
        let theme_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("/theme"))
            .unwrap();
        assert_eq!(
            buf.cell((6, theme_row)).unwrap().style().bg,
            Some(GROKNIGHT.bg_visual)
        );
        let view_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("/view"))
            .unwrap();
        assert_eq!(
            buf.cell((6, view_row)).unwrap().style().bg,
            Some(GROKNIGHT.bg_light)
        );
        // q types into the input instead of quitting
        assert!(!key(&mut app, KeyCode::Char('q')));
        assert!(app.palette.is_some());
        key(&mut app, KeyCode::Esc);
        assert!(app.palette.is_none());
    }

    #[test]
    fn test_palette_theme_switch_persists_and_restyles() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "theme light");
        assert!(!key(&mut app, KeyCode::Enter));
        assert!(app.palette.is_none());
        assert_eq!(app.theme.kind, ThemeKind::Light);
        assert_eq!(
            crate::settings::get_setting("view.theme"),
            Some(json!("light"))
        );
        // the frame really re-renders in grokday
        let buf = draw_to_buffer(&mut app, W, H);
        assert_eq!(buf.cell((0, 0)).unwrap().style().bg, Some(GROKDAY.bg_base));
        // bare /theme cycles back to dark and persists that too
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "theme");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.theme.kind, ThemeKind::Dark);
        assert_eq!(
            crate::settings::get_setting("view.theme"),
            Some(json!("dark"))
        );
    }

    #[test]
    fn test_palette_view_sets_density_and_quit_quits() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "view verbose");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.fold.density, Density::Verbose);
        assert!(app.palette.is_none());
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "quit");
        assert!(key(&mut app, KeyCode::Enter), "/quit exits");
    }

    #[test]
    fn test_palette_find_jumps_and_n_cycles() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("alpha SPEC here"));
        app.push_raw(&assistant_text_row("nothing to see"));
        app.push_raw(&user_row("the spec again"));
        app.push_raw(&assistant_text_row("tail"));
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "find spec");
        key(&mut app, KeyCode::Enter);
        let first = app.selected.expect("find selects a match");
        assert!(app.layout_of(first).unwrap().is_turn);
        key(&mut app, KeyCode::Char('n'));
        let second = app.selected.unwrap();
        assert_ne!(first, second);
        assert!(app.layout_of(second).unwrap().is_turn);
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.selected, Some(first), "wraps around");
        key(&mut app, KeyCode::Char('N'));
        assert_eq!(app.selected, Some(second), "N goes backward");
    }

    #[test]
    fn test_long_user_band_folds_and_expands() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        let long: String = (1..=8)
            .map(|i| format!("user line number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_raw(&user_row(&long));
        app.push_raw(&assistant_text_row("ok"));
        let _ = draw_to_buffer(&mut app, W, H);
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("user line number 1"), "{text}");
        assert!(!text.contains("user line number 8"), "folded at 3 lines");
        assert!(text.contains(" …"), "{text}");
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up); // select the user band
        key(&mut app, KeyCode::Right);
        let text = buffer_text(&draw_to_buffer(&mut app, W, H));
        assert!(text.contains("user line number 8"), "{text}");
    }

    // ---- expanded run: $ command + output band (grok execute.rs) --------

    #[test]
    fn test_expanded_run_shows_dollar_command_and_banded_output() {
        std::env::set_var("TZ", "UTC");
        for t in [&GROKNIGHT, &GROKDAY] {
            let mut app = App::new(t);
            app.push_raw(&user_row("go"));
            app.push_raw(&tool_use_row(
                "Bash",
                "t1",
                json!({"command": "cargo nextest run\n--all-features", "description": "Test"}),
            ));
            app.push_raw(&tool_result_row("t1", "line one\nline two", false));
            app.push_raw(&assistant_text_row("green"));
            let _ = draw_to_buffer(&mut app, W, H);
            key(&mut app, KeyCode::Up); // assistant
            key(&mut app, KeyCode::Up); // run block
            key(&mut app, KeyCode::Right); // expand
            let buf = draw_to_buffer(&mut app, W, H);
            let text = buffer_text(&buf);
            assert!(text.contains("◆ Run Test"), "{text}");
            // `$ ` in gray_dim, the command body in the function-call blue.
            let dollar_row = (0..H)
                .find(|&y| row_text(&buf, y).contains("$ cargo nextest run"))
                .unwrap();
            let dollar = buf.cell((5, dollar_row)).unwrap();
            assert_eq!(dollar.symbol(), "$");
            assert_eq!(dollar.style().fg, Some(t.gray_dim));
            let cmd = buf.cell((7, dollar_row)).unwrap();
            assert_eq!(cmd.symbol(), "c");
            assert_eq!(cmd.style().fg, Some(t.command_fg));
            // physical newline preserved, continuation hangs under the `$ `.
            let cont_row = dollar_row + 1;
            let cont = buf.cell((7, cont_row)).unwrap();
            assert_eq!(cont.symbol(), "-", "{:?}", row_text(&buf, cont_row));
            assert_eq!(cont.style().fg, Some(t.command_fg));
            // exactly one blank spacer row, then the banded output.
            let out_row = (0..H)
                .find(|&y| row_text(&buf, y).contains("line one"))
                .unwrap();
            assert_eq!(out_row, dollar_row + 3, "command, spacer, then output");
            for x in [5u16, 40, W - 5] {
                assert_eq!(
                    buf.cell((x, out_row)).unwrap().style().bg,
                    Some(t.bg_dark),
                    "band bg at ({x},{out_row})"
                );
            }
            assert_eq!(
                buf.cell((5, out_row)).unwrap().style().fg,
                Some(t.text_primary)
            );
            assert!(
                row_text(&buf, out_row + 1).contains("line two"),
                "output line breaks preserved"
            );
            assert_eq!(
                buf.cell((1, out_row)).unwrap().style().bg,
                Some(t.bg_base),
                "outer pad stays on the base bg"
            );
        }
    }

    #[test]
    fn test_expanded_run_error_output_is_unbanded_error_text() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("go"));
        app.push_raw(&tool_use_row(
            "Bash",
            "t1",
            json!({"command": "false", "description": "Fail"}),
        ));
        app.push_raw(&tool_result_row("t1", "exit status 1", true));
        app.push_raw(&assistant_text_row("failed"));
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Right);
        let buf = draw_to_buffer(&mut app, W, H);
        let err_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("exit status 1"))
            .unwrap();
        let cell = buf.cell((5, err_row)).unwrap();
        assert_eq!(cell.style().fg, Some(GROKNIGHT.accent_error));
        assert_eq!(
            cell.style().bg,
            Some(GROKNIGHT.bg_base),
            "no band on errors"
        );
    }

    // ---- composer box ---------------------------------------------------

    #[test]
    fn test_composer_box_rows_present_and_idle() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("hi"));
        app.push_raw(&assistant_text_row("ok"));
        let buf = draw_to_buffer(&mut app, W, H);
        // rows: top border H-5, input H-4, bottom border H-3, hint H-2.
        assert_eq!(buf.cell((2, H - 5)).unwrap().symbol(), "╭");
        assert_eq!(buf.cell((W - 3, H - 5)).unwrap().symbol(), "╮");
        assert_eq!(buf.cell((2, H - 4)).unwrap().symbol(), "│");
        assert_eq!(buf.cell((W - 3, H - 4)).unwrap().symbol(), "│");
        assert_eq!(buf.cell((2, H - 3)).unwrap().symbol(), "╰");
        assert_eq!(buf.cell((W - 3, H - 3)).unwrap().symbol(), "╯");
        // idle: dim border, gray_dim prompt arrow, nothing typed.
        assert_eq!(
            buf.cell((2, H - 5)).unwrap().style().fg,
            Some(GROKNIGHT.prompt_border)
        );
        let arrow = buf.cell((4, H - 4)).unwrap();
        assert_eq!(arrow.symbol(), "❯");
        assert_eq!(arrow.style().fg, Some(GROKNIGHT.gray_dim));
        // idle interior: the prompt arrow plus a faint read-only placeholder.
        let interior: String = (6..W - 3)
            .map(|x| buf.cell((x, H - 4)).unwrap().symbol().to_string())
            .collect();
        assert_eq!(interior.trim(), "read-only", "{interior:?}");
        // model badge embedded on the bottom border, right side.
        let border_row = row_text(&buf, H - 3);
        assert!(border_row.contains("claude-fable-5"), "{border_row:?}");
        assert!(
            border_row.trim_end().ends_with("read-only ─╯"),
            "{border_row:?}"
        );
        // hint row stays below the box.
        assert!(row_text(&buf, H - 2).contains("q:quit"));
    }

    #[test]
    fn test_slash_palette_types_into_composer_box() {
        let mut app = thinking_app();
        let _ = draw_to_buffer(&mut app, W, H);
        key(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "theme");
        let buf = draw_to_buffer(&mut app, W, H);
        let mid = row_text(&buf, H - 4);
        assert!(mid.contains("❯ /theme"), "input inside the box: {mid:?}");
        // focused chrome: active border + accent_user prompt arrow.
        assert_eq!(
            buf.cell((2, H - 5)).unwrap().style().fg,
            Some(GROKNIGHT.prompt_border_active)
        );
        assert_eq!(
            buf.cell((4, H - 4)).unwrap().style().fg,
            Some(GROKNIGHT.accent_user)
        );
        // dropdown panel anchors fully above the box's top border.
        let dropdown_row = (0..H)
            .find(|&y| row_text(&buf, y).contains("/theme"))
            .unwrap();
        assert!(
            dropdown_row < H - 5,
            "dropdown above the box: {dropdown_row}"
        );
        // hint row still rendered under the box while the palette is open.
        assert!(row_text(&buf, H - 2).contains("q:quit"));
        key(&mut app, KeyCode::Esc);
        let buf = draw_to_buffer(&mut app, W, H);
        assert!(
            !row_text(&buf, H - 4).contains("/theme"),
            "box back to idle"
        );
    }

    // ---- markdown tables re-layout on resize ----------------------------

    #[test]
    fn test_markdown_table_relayouts_on_resize() {
        std::env::set_var("TZ", "UTC");
        let mut app = App::new(&GROKNIGHT);
        app.push_raw(&user_row("table"));
        // Natural width well past 120-col wrap budget: the no-width markdown
        // API laid this out at full width and the outer soft-wrap then broke
        // the box-drawing rows after a resize.
        app.push_raw(&assistant_text_row(
            "| alpha column heading one | beta column heading two | gamma column heading three |\n\
             |---|---|---|\n\
             | first cell with a fairly long body | second cell also carrying text | third cell rounding out the row |",
        ));
        let check = |buf: &Buffer, label: &str| {
            let mut saw_frame = false;
            for y in 0..buf.area.height {
                let row = row_text(buf, y);
                for (open, close) in [('┌', '┐'), ('└', '┘'), ('├', '┤')] {
                    if row.contains(open) {
                        saw_frame = true;
                        assert!(
                            row.contains(close),
                            "{label}: table frame row {y} hard-broken: {row:?}"
                        );
                    }
                }
            }
            assert!(saw_frame, "{label}: table frame rendered");
        };
        check(&draw_to_buffer(&mut app, 200, H), "200 cols");
        check(&draw_to_buffer(&mut app, 120, H), "120 cols");
        check(&draw_to_buffer(&mut app, 200, H), "back to 200 cols");
    }
}
