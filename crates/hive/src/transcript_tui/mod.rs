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

mod app;
mod frame;
mod interact;
mod render;
#[cfg(test)]
mod tests;

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
    grok_md, AssistantBlock, DisplayBlock, Entry, HiveMessage, RunBlock, ThinkingBlock, ToolBlock,
    ToolGroupBlock, ToolOutcome, TranscriptParser, UserBlock,
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

use app::*;
#[cfg(test)]
use frame::*;
use render::*;

pub use frame::run;
