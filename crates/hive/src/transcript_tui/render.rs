use super::*;

// ---------------------------------------------------------------------------
// Column-accurate span utilities (unicode-width; CJK never straddles)
// ---------------------------------------------------------------------------

pub(super) type Cell = (char, Style);

pub(super) fn line_cells(line: &Line) -> Vec<Cell> {
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

pub(super) fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|&(ch, _)| cell_width(ch)).sum()
}

/// Rebuild a line, merging adjacent same-style cells into single spans.
pub(super) fn cells_to_line(cells: &[Cell]) -> Line<'static> {
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

/// Punctuation that may not open a line (CJK closing marks and their ASCII
/// cousins) or close one (opening brackets and quotes).
const NO_LINE_START: &str = "，。、；：！？）］｝」』》〉】〕”’…—,.;:!?)]}";
const NO_LINE_END: &str = "（［｛「『《〈【〔“‘([{";

/// Whether a line may break between `prev` and `next`.
///
/// Latin prose breaks at spaces; CJK has none, so a run of Han text used to
/// count as one unbreakable word and got pushed to the next line whole,
/// leaving the line it came from half empty. CJK breaks between any two
/// characters — subject to the punctuation that may not start or end a line.
fn may_break_between(prev: char, next: char) -> bool {
    if next == ' ' || prev == ' ' {
        return true;
    }
    if NO_LINE_START.contains(next) || NO_LINE_END.contains(prev) {
        return false;
    }
    // A path or flag run is breakable after its separators, the way a
    // browser breaks a long URL — otherwise `a/b/c/d…` behaves like one
    // enormous word and strands the line it would not fit on.
    if matches!(prev, '/' | '-' | '_' | '\\') {
        return true;
    }
    cell_width(prev) == 2 || cell_width(next) == 2
}

/// Wrap with a narrower first line: the clock sits at the right end of a
/// block's opening line, and only that line has to make room for it — the
/// rest of the paragraph reflows across the full content width.
fn wrap_cells_first(cells: Vec<Cell>, first: usize, rest: usize) -> Vec<Vec<Cell>> {
    let first = first.max(2);
    let rest = rest.max(2);
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut curw = 0usize;
    for (ch, st) in cells {
        let width = if lines.is_empty() { first } else { rest };
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
            let breaks_here = cur
                .last()
                .is_some_and(|&(prev, _)| may_break_between(prev, ch));
            let split_at = if breaks_here {
                Some(cur.len())
            } else {
                (1..cur.len())
                    .rev()
                    .find(|&i| may_break_between(cur[i - 1].0, cur[i].0))
            };
            if let Some(at) = split_at {
                let rest = cur.split_off(at);
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

pub(super) fn wrap_line(line: &Line, width: usize) -> Vec<Line<'static>> {
    wrap_line_first(line, width, width)
}

fn wrap_line_first(line: &Line, first: usize, rest: usize) -> Vec<Line<'static>> {
    wrap_cells_first(line_cells(line), first, rest)
        .iter()
        .map(|cells| cells_to_line(cells))
        .collect()
}

pub(super) fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    wrap_plain_first(text, width, width)
}

fn wrap_plain_first(text: &str, first: usize, rest: usize) -> Vec<String> {
    let style = Style::default();
    let cells: Vec<Cell> = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .map(|c| (if c == '\t' { ' ' } else { c }, style))
        .collect();
    wrap_cells_first(cells, first, rest)
        .iter()
        .map(|cells| cells.iter().map(|&(c, _)| c).collect())
        .collect()
}

/// Clip to `width` columns; on overflow a `…` inherits the last kept style.
pub(super) fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
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

pub(super) fn clip_plain(text: &str, width: usize) -> String {
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

/// Trim a band's trailing blank lines and, unless expanded, fold it to
/// `BAND_MAX_LINES` with a `…` on the last kept line (clipped to `bw`).
/// Returns whether the band was long enough to fold.
fn fold_band(lines: &mut Vec<String>, bw: usize, expanded: bool) -> bool {
    while lines.last().is_some_and(|l| l.is_empty()) && lines.len() > 1 {
        lines.pop();
    }
    let foldable = lines.len() > BAND_MAX_LINES;
    if foldable && !expanded {
        lines.truncate(BAND_MAX_LINES);
        let last = lines.pop().unwrap_or_default();
        lines.push(format!("{} …", clip_plain(&last, bw.saturating_sub(2))));
    }
    foldable
}

/// One rendered entry plus whether Left/Right can fold it at all.
pub(super) struct Rendered {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) foldable: bool,
}

fn render_user(t: &ViewTheme, u: &UserBlock, inner_w: usize, expanded: bool) -> Rendered {
    let band = Style::default().bg(t.bg_light);
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let bw = cw.saturating_sub(2).max(8);
    let head_w = cw.saturating_sub(TS_RESERVE + 2).max(8);
    if let Some(msg) = u.hive.as_ref() {
        return render_hive(t, u, msg, inner_w, expanded);
    }
    let mut vis: Vec<String> = Vec::new();
    for src in u.text.lines() {
        let first = if vis.is_empty() { head_w } else { bw };
        vis.extend(wrap_plain_first(src, first, bw));
    }
    let foldable = fold_band(&mut vis, bw, expanded);
    let mut out = Vec::new();
    // No marker: the band's own background is what says this is the human.
    out.push(band_line(t, Vec::new(), inner_w)); // vpad top
    for (i, text) in vis.iter().enumerate() {
        let mut spans = vec![Span::styled(
            text.clone(),
            fg(t.text_primary).bg(t.bg_light),
        )];
        if i == 0 {
            if let Some(ts) = u.timestamp.as_ref() {
                let used = UnicodeWidthStr::width(text.as_str());
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

/// How a peer message separates itself from an ordinary user band: the
/// contrast is structural — a user message is a filled band, a peer's is an
/// open rail on the base background with an accent glyph in the margin.
fn hive_band_style(t: &ViewTheme) -> (Color, char, Color) {
    (t.bg_base, '▏', t.accent_model)
}

/// Band row for a peer message: rail glyph in the margin, everything on the
/// hive band colour.
fn hive_line(
    bg: Color,
    rail: char,
    rail_fg: Color,
    spans: Vec<Span<'static>>,
    inner_w: usize,
) -> Line<'static> {
    let band = Style::default().bg(bg);
    let mut all = vec![
        Span::styled(" ".to_string(), band),
        Span::styled(rail.to_string(), Style::default().fg(rail_fg).bg(bg)),
        Span::styled(" ".to_string(), band),
    ];
    all.extend(spans);
    let line = Line::from(all);
    let used = cells_width(&line_cells(&line));
    let mut all = line.spans;
    if inner_w > used {
        all.push(Span::styled(" ".repeat(inner_w - used), band));
    }
    Line::from(all)
}

/// A HIVE envelope. The tag becomes a header — the sender, ids and clock
/// on the right — the body reads as plain text, and `artifact=` gets its own
/// line. Whatever wrapper carried it (claude's peer-message injection, the
/// retired `<channel>` block) never reaches the screen. The whole block sits
/// on its own band so peer traffic never reads as something the human typed.
fn render_hive(
    t: &ViewTheme,
    u: &UserBlock,
    msg: &HiveMessage,
    inner_w: usize,
    expanded: bool,
) -> Rendered {
    let (bg, rail, rail_fg) = hive_band_style(t);
    let band = Style::default().bg(bg);
    let line = |spans: Vec<Span<'static>>| hive_line(bg, rail, rail_fg, spans, inner_w);
    let cw = inner_w.saturating_sub(ROW_CHROME);
    let bw = cw.saturating_sub(2).max(8);

    let mut head: Vec<Span<'static>> = Vec::new();
    if let Some(icon) = msg.icon {
        head.push(Span::styled(format!("{icon} "), fg(rail_fg).bg(bg)));
    }
    head.push(Span::styled(
        msg.from.clone().unwrap_or_else(|| "peer".to_string()),
        fg(rail_fg).bg(bg).add_modifier(Modifier::BOLD),
    ));
    let mut tail = String::new();
    if let Some(r) = msg.reply_to.as_deref() {
        tail.push_str(&format!("↩{r} "));
    }
    if let Some(id) = msg.msg_id.as_deref() {
        tail.push_str(id);
    }
    if let Some(ts) = u.timestamp.as_ref() {
        if !tail.is_empty() {
            tail.push_str("  ");
        }
        tail.push_str(&ts.clock);
    }
    let head_w: usize = head
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if !tail.is_empty() {
        let tail_w = UnicodeWidthStr::width(tail.as_str());
        if cw > head_w + tail_w + 2 {
            head.push(Span::styled(" ".repeat(cw - head_w - tail_w), band));
            head.push(Span::styled(tail, fg(t.gray).bg(bg)));
        }
    }

    let mut body: Vec<String> = Vec::new();
    for src in msg.body.lines() {
        body.extend(wrap_plain(src, bw));
    }
    let foldable = fold_band(&mut body, bw, expanded);

    let mut out = vec![line(Vec::new()), line(head)];
    for text in body {
        out.push(line(vec![Span::styled(text, fg(t.text_primary).bg(bg))]));
    }
    if let Some(path) = msg.artifact.as_deref() {
        out.push(line(vec![
            Span::styled("↳ ".to_string(), fg(t.gray).bg(bg)),
            Span::styled(clip_plain(path, bw.saturating_sub(2)), fg(t.link_fg).bg(bg)),
        ]));
    }
    out.push(line(Vec::new()));
    Rendered {
        lines: out,
        foldable,
    }
}

fn render_assistant(t: &ViewTheme, a: &AssistantBlock, inner_w: usize) -> Rendered {
    let cw = inner_w.saturating_sub(ROW_CHROME);
    // Only the opening line gives up columns to the clock.
    let aw = cw.max(10);
    let head_w = cw.saturating_sub(TS_RESERVE).max(10);
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in grok_md::render_ratatui(&a.markdown, t, aw) {
        // Narrowing the opening line only works where there is a space to
        // break on. A table's frame row has none, so it would be sawn in
        // half; leave those at full width and let the clock drop instead.
        let breakable = line_cells(&line).iter().any(|&(c, _)| c == ' ');
        let first = if wrapped.is_empty() && breakable {
            head_w
        } else {
            aw
        };
        wrapped.extend(wrap_line_first(&line, first, aw));
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

/// grok colours an execution bullet by its outcome: green once it came back
/// clean, red when it errored, grey while it is still running.
fn outcome_color(t: &ViewTheme, result: &Option<crate::transcript_view::ToolOutcome>) -> Color {
    match result {
        Some(r) if r.is_error => t.accent_error,
        Some(_) => t.accent_success,
        None => t.gray,
    }
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

/// The scrollback's left margin; the full-screen block viewer has none.
const SCROLLBACK_INDENT: &str = "   ";

fn indented(indent: &str, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = Vec::with_capacity(spans.len() + 1);
    if !indent.is_empty() {
        all.push(Span::raw(indent.to_string()));
    }
    all.extend(spans);
    Line::from(all)
}

/// One tool-output band row (grok execute.rs `.with_panel_background`): the
/// text on a `width`-column `bg_dark` band after `indent`.
fn band_row(
    t: &ViewTheme,
    indent: &str,
    text: String,
    text_fg: Color,
    width: usize,
) -> Line<'static> {
    let used = UnicodeWidthStr::width(text.as_str());
    let mut spans = vec![Span::styled(text, fg(text_fg).bg(t.bg_dark))];
    if width > used {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(t.bg_dark),
        ));
    }
    indented(indent, spans)
}

/// Expanded tool output (grok execute.rs render_with_truncation): success —
/// `bg_dark` band rows in primary text preserving line breaks; error —
/// accent_error text with no band (grok's error branch). The storage-cap
/// marker renders muted. Shared by the scrollback and the block viewer,
/// which differ in `indent`, `width`, and the scrollback's leading spacer
/// (`scrollback_outcome`).
pub(super) fn outcome_rows(
    t: &ViewTheme,
    out: &mut Vec<Line<'static>>,
    indent: &str,
    res: &ToolOutcome,
    width: usize,
) {
    let text = res.text.trim_end();
    if res.is_error {
        for src in text.lines() {
            for piece in wrap_plain(src, width) {
                out.push(indented(
                    indent,
                    vec![Span::styled(piece, fg(t.accent_error))],
                ));
            }
        }
        if res.truncated {
            out.push(indented(
                indent,
                vec![Span::styled("… output truncated".to_string(), fg(t.gray))],
            ));
        }
        return;
    }
    for src in text.lines() {
        for piece in wrap_plain(src, width) {
            out.push(band_row(t, indent, piece, t.text_primary, width));
        }
    }
    if res.truncated {
        out.push(band_row(
            t,
            indent,
            "… output truncated".to_string(),
            t.gray,
            width,
        ));
    }
}

/// Scrollback outcome: one blank spacer, then the rows, only when there is
/// anything to show.
fn scrollback_outcome(
    t: &ViewTheme,
    out: &mut Vec<Line<'static>>,
    res: &ToolOutcome,
    inner_w: usize,
) {
    let mut rows = Vec::new();
    outcome_rows(
        t,
        &mut rows,
        SCROLLBACK_INDENT,
        res,
        inner_w.saturating_sub(ROW_CHROME),
    );
    if !rows.is_empty() {
        out.push(Line::default());
        out.extend(rows);
    }
}

/// Expanded Run command lines (grok execute.rs push_command_soft_wrap): `$ `
/// in gray_dim, the command body in the theme's bash function-call blue,
/// physical newlines preserved, soft-wrap and continuation rows hanging
/// 2 cols so they align under the command after the `$ `.
pub(super) fn command_rows(
    t: &ViewTheme,
    out: &mut Vec<Line<'static>>,
    indent: &str,
    command: &str,
    width: usize,
) {
    let bw = width.saturating_sub(2).max(1);
    let mut first = true;
    for src in command.lines() {
        for piece in wrap_plain(src, bw) {
            let lead = if first { "$ " } else { "  " };
            first = false;
            out.push(indented(
                indent,
                vec![
                    Span::styled(lead.to_string(), fg(t.gray_dim)),
                    Span::styled(piece, fg(t.command_fg)),
                ],
            ));
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
    let bullet_color = if failed > 0 {
        t.accent_error
    } else if g.members.iter().all(|m| m.result.is_some()) {
        t.accent_success
    } else {
        t.gray
    };
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
            let bullet = outcome_color(t, &member.result);
            let spans = vec![
                Span::raw("   "),
                Span::styled("◆ ", fg(bullet)),
                Span::styled(member.name.clone(), bold(t.gray)),
                Span::styled(format!("  {}", member.hint), fg(t.gray)),
            ];
            lines.push(clip_spans(spans, inner_w));
            if let Some(res) = &member.result {
                scrollback_outcome(t, &mut lines, res, inner_w);
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
    let bullet_color = outcome_color(t, &r.result);
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
        command_rows(
            t,
            &mut lines,
            SCROLLBACK_INDENT,
            &r.command,
            inner_w.saturating_sub(ROW_CHROME),
        );
        if let Some(res) = &r.result {
            scrollback_outcome(t, &mut lines, res, inner_w);
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
    let bullet_color = outcome_color(t, &tool.result);
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
            scrollback_outcome(t, &mut lines, res, inner_w);
        }
    }
    Rendered {
        lines,
        foldable: true,
    }
}

pub(super) fn render_entry(
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

pub(super) fn fold_kind(block: &DisplayBlock) -> FoldKind {
    match block {
        DisplayBlock::Thinking(_) => FoldKind::Thinking,
        DisplayBlock::ToolGroup(_) | DisplayBlock::Run(_) | DisplayBlock::Tool(_) => FoldKind::Tool,
        DisplayBlock::User(_) => FoldKind::User,
        DisplayBlock::Assistant(_) | DisplayBlock::WorkedFor(_) => FoldKind::Fixed,
    }
}

/// The text `/find` matches against, per block.
pub(super) fn search_text(block: &DisplayBlock) -> String {
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
