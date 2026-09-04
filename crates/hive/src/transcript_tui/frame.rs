use super::*;
use crate::transcript_view::LineAccumulator;

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

/// Human model name the way the CLIs badge themselves: "claude-fable-5" →
/// "Fable 5", "claude-haiku-4-5-20251001" → "Haiku 4.5". Unknown shapes pass
/// through untouched.
pub(super) fn display_model(id: &str) -> String {
    let Some(rest) = id.strip_prefix("claude-") else {
        return id.to_string();
    };
    let mut words: Vec<String> = Vec::new();
    let mut version: Vec<String> = Vec::new();
    for seg in rest.split('-') {
        if seg.chars().all(|c| c.is_ascii_digit()) {
            if seg.len() >= 8 {
                break; // a datestamp tail, not a version part
            }
            version.push(seg.to_string());
        } else {
            let mut chars = seg.chars();
            let cap = match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => continue,
            };
            words.push(cap);
        }
    }
    if !version.is_empty() {
        words.push(version.join("."));
    }
    if words.is_empty() {
        id.to_string()
    } else {
        words.join(" ")
    }
}

/// grok's braille turn spinner (`⠋⠙⠹⠸⠼⠴⠦⠧`, xai-grok-pager-render
/// glyphs.rs::braille_spinner_frames), stepped at its ~7.5fps cadence off
/// wall-clock rather than a frame counter, since this viewer only redraws
/// on its poll interval.
pub(super) const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const SPINNER_MS: i64 = 133;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What the mirrored session is doing right now, named the way grok names it
/// (views/turn_status.rs): the innermost unfinished block if there is one,
/// otherwise the assistant is writing.
fn activity_label(app: &App) -> String {
    match app.parser.pending_entries().last().map(|e| e.block.clone()) {
        Some(DisplayBlock::Thinking(_)) => "Thinking…".to_string(),
        Some(DisplayBlock::Run(r)) if !r.description.is_empty() => format!("{}…", r.description),
        Some(DisplayBlock::Run(_)) => "Running…".to_string(),
        Some(DisplayBlock::Tool(tool)) => format!("{}…", tool.name),
        Some(DisplayBlock::ToolGroup(g)) => format!("{}…", g.label()),
        _ => "Responding…".to_string(),
    }
}

/// grok's turn-status row (views/turn_status.rs): `⠧ Thinking… 3s` on the
/// left, the turn timer and token count on the right, and nothing at all
/// while idle. No `[stop]` — this viewer is read-only, there is nothing here
/// to cancel.
fn running_line(app: &App, inner_w: usize) -> Option<Line<'static>> {
    if !app.parser.busy() {
        return None;
    }
    let t = app.theme;
    let elapsed_ms = app
        .parser
        .turn_started_ms()
        .map(|start| (now_ms() - start).max(0))
        .unwrap_or(0);
    let frame = SPINNER[((elapsed_ms / SPINNER_MS) as usize) % SPINNER.len()];
    let secs = elapsed_ms as f64 / 1000.0;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!("{frame} "), fg(t.accent_model)),
        Span::styled(activity_label(app), fg(t.text_secondary)),
        Span::styled(
            format!(" {}", crate::transcript_view::format_worked_duration(secs)),
            fg(t.gray),
        ),
    ];
    let tokens = app.parser.tokens();
    if tokens > 0 {
        let right = format!("⇣{}", fmt_tokens(tokens));
        let used = cells_width(&line_cells(&Line::from(spans.clone())));
        let right_w = UnicodeWidthStr::width(right.as_str());
        if inner_w > used + right_w + 1 {
            spans.push(Span::raw(" ".repeat(inner_w - used - right_w - 1)));
            spans.push(Span::styled(right, fg(t.gray)));
        }
    }
    Some(clip_spans(spans, inner_w))
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
        ("Enter", "open"),
        ("Ctrl+o", "view"),
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

/// grok's scrollbar (xai-grok-pager-render render/scrollbar.rs): the same
/// `tui-scrollbar` crate places the thumb at sub-cell precision, and every
/// covered cell is then flattened to a solid `█` — some emulators do not
/// stretch a fg-only block over the cell's line gap, so the thumb fills the
/// background too. Following dims the thumb toward the track; scrolled up it
/// stands at full strength, which is how grok shows you have left the tail.
fn draw_scrollbar(
    buf: &mut Buffer,
    t: &ViewTheme,
    track: Rect,
    total: usize,
    offset: usize,
    following: bool,
) {
    if track.width == 0 || track.height == 0 || total <= track.height as usize {
        return;
    }
    // The widget takes u16 lengths; long transcripts scale down to fit.
    let scale = (total / u16::MAX as usize) + 1;
    let lengths = tui_scrollbar::ScrollLengths {
        content_len: total / scale,
        viewport_len: track.height as usize,
    };
    let metrics = tui_scrollbar::ScrollMetrics::new(lengths, offset / scale, track.height);
    let thumb = if following {
        blend(t.scrollbar_bg, t.scrollbar_fg, 0.4)
    } else {
        t.scrollbar_fg
    };
    for row in 0..track.height {
        let (x, y) = (track.x, track.y + row);
        let empty = matches!(
            metrics.cell_fill(row as usize),
            tui_scrollbar::CellFill::Empty
        );
        let cell = &mut buf[(x, y)];
        if empty {
            cell.set_symbol(" ");
            cell.set_style(Style::default().bg(t.scrollbar_bg));
        } else {
            cell.set_symbol("█");
            cell.set_style(Style::default().fg(thumb).bg(thumb));
        }
    }
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
        // "─ Grok 4.6 (xhigh) · always-approve ─". Ours carries model,
        // effort, and the Ctrl+O density mode behind them.
        let mut badge: Vec<Span<'static>> = Vec::new();
        if let Some(m) = app.parser.model() {
            badge.push(Span::styled(
                format!(" {}", display_model(m)),
                fg(t.accent_model),
            ));
            if let Some(e) = app.parser.effort() {
                badge.push(Span::styled(format!(" ({e})"), fg(t.gray)));
            }
            badge.push(Span::styled(
                " · ".to_string(),
                fg(t.gray).add_modifier(Modifier::DIM),
            ));
        }
        badge.push(Span::styled(
            format!("{} ", app.fold.density.label()),
            fg(t.gray),
        ));
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
        "Enter close · j/k scroll · g/G ends".to_string(),
        fg(t.gray),
    ));
    frame.render_widget(Paragraph::new(hint).style(base), footer);
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
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
    // The composer box floats between the scrollback and the hint row with a
    // one-row breather on BOTH sides (grok spacing), and the turn-status row
    // sits between that breather and the box — always reserved, blank while
    // idle, so a turn starting does not shove the transcript up a line. A
    // pane too short for status + gap + one scroll row + breather + running +
    // box + breather + hint drops the box.
    let have_box = inner.height >= 2 + gap + COMPOSER_H + 4;
    let box_rect = have_box.then(|| Rect {
        x: inner.x,
        y: hint_rect.y - 1 - COMPOSER_H,
        width: inner.width,
        height: COMPOSER_H,
    });
    let reserved = 2 + gap + if have_box { COMPOSER_H + 3 } else { 0 };
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
    if hpad > 0 {
        // The track rides the page's right margin: grok's gap + 1-col track,
        // and the transcript keeps every column it had.
        let track = Rect {
            x: area.x + area.width - 1,
            y: scroll_rect.y,
            width: 1,
            height: scroll_rect.height,
        };
        draw_scrollbar(
            frame.buffer_mut(),
            t,
            track,
            lines.len(),
            app.scroll.offset,
            app.scroll.follow,
        );
    }
    if let Some(le) = app.selected.and_then(|id| app.layout_of(id)) {
        draw_selection_frame(frame.buffer_mut(), t, scroll_rect, app.scroll.offset, le);
    }
    frame.render_widget(
        Paragraph::new(top_line(app, inner.width as usize)).style(base),
        status_rect,
    );
    if let Some(bx) = box_rect {
        // The row directly above the box; the breather is the one above that.
        if let Some(line) = running_line(app, inner.width as usize) {
            frame.render_widget(
                Paragraph::new(line).style(base),
                Rect {
                    x: inner.x,
                    y: bx.y - 1,
                    width: inner.width,
                    height: 1,
                },
            );
        }
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
    let mut backlog = Vec::new();
    reader.read_to_end(&mut backlog)?;
    let mut app = App::new(theme);
    let mut lines = LineAccumulator::new();
    load_backlog(&mut app, &mut lines, &backlog);

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
    let result = event_loop(&mut terminal, &mut app, &mut lines, &mut reader);
    restore_terminal();
    result.map(|_| 0)
}

/// Seed the app from the backlog read at open: whole rows before the tail
/// only feed session state, the tail is parsed, a trailing partial row waits
/// in `lines` for the follow loop.
pub(super) fn load_backlog(app: &mut App, lines: &mut LineAccumulator, backlog: &[u8]) {
    let whole = lines.split_backlog(backlog);
    let tail_from = whole.len().saturating_sub(TAIL_EVENTS);
    for raw in &whole[..tail_from] {
        app.parser.note_session_state(raw);
    }
    for raw in &whole[tail_from..] {
        app.push_raw(raw);
    }
}

/// Read everything appended since the last poll; only whole rows reach the
/// parser.
pub(super) fn drain_reader<R: BufRead>(
    app: &mut App,
    lines: &mut LineAccumulator,
    reader: &mut R,
) -> io::Result<()> {
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                if let Some(line) = lines.push(&raw) {
                    app.push_raw(&line);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(err) => return Err(err),
        }
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    lines: &mut LineAccumulator,
    reader: &mut BufReader<File>,
) -> anyhow::Result<()> {
    loop {
        drain_reader(app, lines, reader)?;
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
