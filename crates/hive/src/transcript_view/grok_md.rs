use std::sync::OnceLock;
use xai_grok_markdown::{MarkdownStyle, Syntect};

use crate::view_theme::{ThemeKind, ViewTheme, GROKNIGHT};

type Style = anstyle::Style;

fn ans(c: ratatui::style::Color) -> anstyle::Color {
    match c {
        ratatui::style::Color::Rgb(r, g, b) => anstyle::Color::Rgb(anstyle::RgbColor(r, g, b)),
        _ => anstyle::Color::Rgb(anstyle::RgbColor(0, 0, 0)),
    }
}

fn fg(c: anstyle::Color) -> Style {
    Style::new().fg_color(Some(c))
}

/// grok's MarkdownStyle built from the theme's md_* fields.
fn style(t: &ViewTheme) -> MarkdownStyle {
    let heading_colors = t.md_heading.map(ans);
    let mut heading_inner = heading_colors.map(fg);
    for s in heading_inner.iter_mut().take(5) {
        *s = s.bold();
    }
    let text = ans(t.md_text);
    let code = ans(t.md_code);
    let muted = ans(t.md_muted);
    MarkdownStyle {
        heading_inner,
        heading_outer: heading_colors.map(|c| fg(c).dimmed().hidden()),
        strong_inner: fg(text).bold(),
        strong_outer: Style::new().dimmed().hidden(),
        emphasis_inner: fg(text).italic(),
        emphasis_outer: Style::new().dimmed().hidden(),
        strikethrough_inner: fg(text).strikethrough(),
        strikethrough_outer: Style::new().dimmed().hidden(),
        inline_code_inner: fg(code).bold(),
        inline_code_outer: fg(code).dimmed().hidden(),
        blockquote_outer: fg(muted).dimmed(),
        task_checked: fg(ans(t.md_task_checked)),
        task_unchecked: fg(ans(t.md_task_unchecked)).dimmed(),
        list_item: fg(muted),
        rule: fg(muted),
        link_outer: fg(muted),
        link_text: fg(ans(t.link_fg)).underline(),
        link_url: fg(muted),
        link_title: fg(muted),
        code_outer: fg(code).dimmed().hidden(),
        code_language: fg(ans(t.md_code_language)).hidden(),
        code_untagged: fg(text),
        code_background: Style::new().bg_color(Some(ans(t.md_code_bg))),
        table_outer: fg(ans(t.md_table)).hidden(),
        text: fg(text),
        math: fg(text).italic(),
    }
}

/// grok syntax.rs::get_syntect: GrokNight ships grok-night.tmTheme,
/// GrokDay ships grok-day.tmTheme; both vendored verbatim.
fn syntect(kind: ThemeKind) -> &'static Syntect {
    static NIGHT: OnceLock<Syntect> = OnceLock::new();
    static DAY: OnceLock<Syntect> = OnceLock::new();
    match kind {
        ThemeKind::Dark => {
            NIGHT.get_or_init(|| Syntect::new(include_bytes!("../../assets/grok-night.tmTheme")))
        }
        ThemeKind::Light => {
            DAY.get_or_init(|| Syntect::new(include_bytes!("../../assets/grok-day.tmTheme")))
        }
    }
}

/// Render markdown to an ANSI string, trailing whitespace trimmed. The
/// plain piped stream has no terminal to detect and keeps the groknight
/// look.
pub fn render(text: &str) -> String {
    let (out, _) = xai_grok_markdown::render_markdown(
        text,
        style(&GROKNIGHT),
        true,
        Some(syntect(ThemeKind::Dark)),
    );
    out.trim_end().to_string()
}

/// Render markdown to ratatui lines (the TUI mirror) in `theme`, with
/// tables constrained to `width` display columns. The width MUST be the
/// caller's current content width: the simple no-width API lays tables
/// out at their natural width and the caller's outer soft-wrap then
/// hard-breaks the box-drawing rows (the resize-collapse bug).
pub fn render_ratatui(
    text: &str,
    theme: &ViewTheme,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let mut buffers = xai_grok_markdown::MarkdownBuffers::new();
    let (out, _) = xai_grok_markdown::render_markdown_ratatui_with_buffers_width(
        text,
        style(theme),
        true,
        &mut buffers,
        Some(syntect(theme.kind)),
        Some(width.max(4)),
    );
    out.lines
        .into_iter()
        .map(|l| frame_table(l, theme))
        .collect()
}

/// The engine paints a table's verticals with the muted+dim blockquote
/// style but leaves its horizontals unstyled, so the `│` column reads
/// broken: faint between rows, full-strength wherever a `─` rule crosses
/// it. One style for the whole frame.
fn frame_table(
    line: ratatui::text::Line<'static>,
    theme: &ViewTheme,
) -> ratatui::text::Line<'static> {
    const BOX: &str = "─│┌┐└┘├┤┬┴┼";
    let frame = ratatui::style::Style::default().fg(theme.md_muted);
    let spans = line
        .spans
        .into_iter()
        .map(|s| {
            if !s.content.is_empty() && s.content.chars().all(|c| BOX.contains(c)) {
                ratatui::text::Span::styled(s.content, frame)
            } else {
                s
            }
        })
        .collect::<Vec<_>>();
    ratatui::text::Line::from(spans)
}
