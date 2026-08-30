//! Read-only live mirror for a Claude session transcript.
//!
//! An interactive Claude session (a desktop ccd, a joined session) has no
//! attachable pty — `claude attach` is job-only, and resuming would fork a
//! second engine. Its truth layer is the transcript JSONL, appended event by
//! event as the turn unfolds, so a faithful renderer over that file IS the
//! mirror: native-looking, keystrokes go nowhere by construction.
//!
//! Parse layer: [`TranscriptParser`] folds raw JSONL lines into typed
//! [`DisplayBlock`]s (user band, aggregated tool group, run, thinking,
//! assistant markdown, worked-for). Blocks can finalize late — a tool group
//! stays open until a non-read event arrives — so the parser exposes both the
//! finalized stream (`push`/`flush`) and a live snapshot (`pending_blocks`).
//! The TUI renders the blocks; the plain non-tty stream below renders the
//! same blocks to the legacy ANSI line format.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";
const YELLOW: &str = "\x1b[33m";
const CLEAR_LINE: &str = "\x1b[2K\r";

const _TAIL_EVENTS: usize = 40;
const _POLL_SECONDS: f64 = 0.25;
const _SPINNER: &str = "✻✼✢✽";
/// Idle polls before the plain stream force-finalizes pending blocks.
const _IDLE_FLUSH_TICKS: usize = 4;
/// Storage cap for one tool result's full text; longer results are cut at a
/// char boundary and flagged `truncated`.
pub const TOOL_RESULT_MAX_BYTES: usize = 512 * 1024;

pub fn transcript_path(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects = Path::new(&home).join(".claude").join("projects");
    let entries = std::fs::read_dir(&projects).ok()?;
    let mut matches: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if let Ok(meta) = std::fs::metadata(&candidate) {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            matches.push((mtime, candidate));
        }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    matches.into_iter().next().map(|(_, p)| p)
}

fn _clip(text: &str, limit: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(limit) {
        None => text.to_string(),
        Some((cut, _)) => format!("{} …", &text[..cut]),
    }
}

/// Grok Build's markdown engine (xai-grok-markdown, Apache-2.0) with the
/// palette derived from the active [`ViewTheme`] (groknight or grokday) —
/// syntax highlighting, tables, headings, the whole surface.
pub(crate) mod grok_md {
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
                NIGHT.get_or_init(|| Syntect::new(include_bytes!("../assets/grok-night.tmTheme")))
            }
            ThemeKind::Light => {
                DAY.get_or_init(|| Syntect::new(include_bytes!("../assets/grok-day.tmTheme")))
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
    }
}

fn _md(text: &str) -> String {
    grok_md::render(text)
}

fn _indent_block(text: &str, first: &str, rest: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        lines.push("");
    }
    let mut out = format!("{}{}", first, lines[0]);
    for line in &lines[1..] {
        out.push('\n');
        out.push_str(rest);
        out.push_str(line);
    }
    out
}

/// A HIVE envelope as it reaches a claude transcript, with the tag parsed
/// into fields instead of shown raw.
///
/// Four carriers exist, all of which land in the same `user` row:
/// bare (typed straight into the pane), claude's session-inbox injection at
/// turn start or folded in mid-turn (a lead line plus a trailing safety
/// paragraph), and the retired `<channel …>` wrapper still sitting in old
/// transcripts.
#[derive(Debug, Clone, PartialEq)]
pub struct HiveMessage {
    pub from: Option<String>,
    pub to: Option<String>,
    pub msg_id: Option<String>,
    pub reply_to: Option<String>,
    pub artifact: Option<String>,
    pub body: String,
    /// The envelope arrived inside claude's peer-message wrapper rather than
    /// on its own.
    pub injected: bool,
    /// The wrapper said the message folded into a turn already in flight.
    pub mid_turn: bool,
}

const INJECT_LEAD_MID: &str = "Another Claude session sent a message while you were working:";
const INJECT_LEAD: &str = "Another Claude session sent a message:";
const INJECT_TAIL: &str = "This came from another Claude session";

/// Peel the retired `<channel source=… msg_id=…>` wrapper.
fn strip_channel_wrapper(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("<channel") else {
        return text;
    };
    let Some(gt) = rest.find('>') else {
        return text;
    };
    let inner = rest[gt + 1..].trim();
    inner.strip_suffix("</channel>").unwrap_or(inner).trim()
}

/// Peel claude's peer-message wrapper: the lead line above the envelope and
/// the safety paragraph below it. Returns the core plus (injected, mid_turn).
fn strip_injection_wrapper(text: &str) -> (&str, bool, bool) {
    let trimmed = strip_channel_wrapper(text.trim());
    for (lead, mid) in [(INJECT_LEAD_MID, true), (INJECT_LEAD, false)] {
        if let Some(rest) = trimmed.strip_prefix(lead) {
            let rest = rest.trim_start();
            let core = match rest.find(INJECT_TAIL) {
                Some(i) => &rest[..i],
                None => rest,
            };
            return (core.trim(), true, mid);
        }
    }
    (trimmed, false, false)
}

/// Parse a user row's text as one HIVE envelope, in any of its carriers.
///
/// Deliberately strict: the row must be *nothing but* the envelope once the
/// wrapper is peeled, so prose that merely quotes `<HIVE …>` — skill docs,
/// this repo's own specs — stays ordinary user text.
pub(crate) fn parse_hive_message(text: &str) -> Option<HiveMessage> {
    let (core, injected, mid_turn) = strip_injection_wrapper(text);
    if !core.starts_with("<HIVE") {
        return None;
    }
    let body_end = core.strip_suffix("</HIVE>")?.len();
    let gt = core.find('>')?;
    if gt >= body_end {
        return None;
    }
    let tag = &core[5..gt];
    if !tag.is_empty() && !tag.starts_with(char::is_whitespace) {
        return None;
    }
    let mut msg = HiveMessage {
        from: None,
        to: None,
        msg_id: None,
        reply_to: None,
        artifact: None,
        body: core[gt + 1..body_end].trim().to_string(),
        injected,
        mid_turn,
    };
    for attr in tag.split_whitespace() {
        let Some((key, value)) = attr.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let slot = match key {
            "from" => &mut msg.from,
            "to" => &mut msg.to,
            "msgId" => &mut msg.msg_id,
            "reply-to" => &mut msg.reply_to,
            "artifact" => &mut msg.artifact,
            _ => continue,
        };
        *slot = Some(value.to_string());
    }
    Some(msg)
}

// ---------------------------------------------------------------------------
// Timestamps & durations
// ---------------------------------------------------------------------------

/// A transcript row instant: raw epoch for duration math plus the local
/// `h:MM AM/PM` clock string the grok chrome shows (no leading spaces — the
/// renderer owns alignment padding).
#[derive(Debug, Clone, PartialEq)]
pub struct Timestamp {
    pub epoch_ms: i64,
    pub clock: String,
}

/// Days since 1970-01-01 (Howard Hinnant's civil-days algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse ISO-8601 `YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH[:MM]]` to epoch millis.
fn iso_to_epoch_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut idx = 19;
    let mut ms = 0i64;
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        let frac: String = s[start..idx].chars().take(3).collect();
        ms = format!("{frac:0<3}").parse().ok()?;
    }
    let mut offset = 0i64;
    if let Some(&c) = b.get(idx) {
        if c == b'+' || c == b'-' {
            let sign = if c == b'+' { 1 } else { -1 };
            let oh = num(idx + 1..idx + 3)?;
            let om = if b.get(idx + 3) == Some(&b':') {
                num(idx + 4..idx + 6).unwrap_or(0)
            } else {
                num(idx + 3..idx + 5).unwrap_or(0)
            };
            offset = sign * (oh * 3600 + om * 60);
        }
    }
    Some((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset) * 1000 + ms)
}

extern "C" {
    /// Not exposed by the `libc` crate on macOS; declared directly.
    fn tzset();
}

/// Local wall clock as `12:40 PM` (`%-I:%M %p`: 12-hour, no leading zero).
fn clock_string(epoch_secs: i64) -> String {
    unsafe {
        tzset();
        let t: libc::time_t = epoch_secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let (hour, ampm) = match tm.tm_hour {
            0 => (12, "AM"),
            h @ 1..=11 => (h, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{}:{:02} {}", hour, tm.tm_min, ampm)
    }
}

fn parse_timestamp(s: &str) -> Option<Timestamp> {
    let epoch_ms = iso_to_epoch_ms(s)?;
    Some(Timestamp {
        epoch_ms,
        clock: clock_string(epoch_ms.div_euclid(1000)),
    })
}

/// Thinking-header duration (grok thinking.rs::format_time):
/// `< 60s` → `14.3s`; else `1m12s` (`{}m{:.0}s`).
pub fn format_thinking_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let mins = (secs / 60.0) as u64;
        let rem = secs - (mins as f64) * 60.0;
        format!("{mins}m{rem:.0}s")
    }
}

/// Worked-for duration (grok util.rs::format_duration):
/// `< 10s` → `4.4s`; `< 60s` → `32s`; `< 1h` → `4m6s`; else `1h2m`.
pub fn format_worked_duration(secs: f64) -> String {
    if secs < 10.0 {
        return format!("{secs:.1}s");
    }
    let s = secs as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

// ---------------------------------------------------------------------------
// Display-block model
// ---------------------------------------------------------------------------

/// One visual block of the mirror, in transcript order. The model is dumb and
/// complete: raw text/markdown plus enough metadata for the TUI to style it;
/// rendering (colors, wrapping, ellipsis) happens in the consumer.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayBlock {
    /// Full-width band: `❯` prefix, right-aligned timestamp on the first line.
    User(UserBlock),
    /// `◈ Read 1 file, Searched 1 pattern` — consecutive read-only tools.
    ToolGroup(ToolGroupBlock),
    /// `◆ Run <description>` — a Bash invocation.
    Run(RunBlock),
    /// `◆ <Name> <hint>` — any other tool.
    Tool(ToolBlock),
    /// `◆ Thought for 14.3s`.
    Thinking(ThinkingBlock),
    /// Assistant markdown, right-aligned timestamp on the first line.
    Assistant(AssistantBlock),
    /// Muted `Worked for 4m6s` turn-end marker.
    WorkedFor(WorkedForBlock),
}

impl DisplayBlock {
    /// True when this block starts a turn — a turn spans a user prompt up to
    /// the next one (grok nav.rs rebuild_turns), so User blocks are the turn
    /// anchors Shift+Left/Right jump between.
    pub fn starts_turn(&self) -> bool {
        matches!(self, DisplayBlock::User(_))
    }
}

/// A [`DisplayBlock`] plus its stable identity. Ids increase monotonically in
/// birth order (which matches display order), are assigned once — when the
/// block first appears, pending or immediate — and survive finalization, so
/// the TUI can key selection/fold state on them across live re-parses.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub block: DisplayBlock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserBlock {
    /// Raw message text; for HIVE envelopes this still holds the full
    /// source, wrapper and all — `hive` carries the parsed view.
    pub text: String,
    pub hive: Option<HiveMessage>,
    pub timestamp: Option<Timestamp>,
}

/// Aggregation bucket kind for read-only tools (grok VerbGroupKind subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// `Read` → file/files
    File,
    /// SKILL.md reads and Skill invocations → skill/skills
    Skill,
    /// `Grep`/`Glob` → pattern/patterns
    Search,
    /// `LS` → dir/dirs
    Dir,
    /// `WebFetch` → website/websites
    WebFetch,
    /// `WebSearch` → website/websites
    WebSearch,
}

impl GroupKind {
    /// Past-tense verb (the static mirror only shows completed runs).
    pub fn verb(self) -> &'static str {
        match self {
            GroupKind::File | GroupKind::Skill => "Read",
            GroupKind::Search | GroupKind::WebSearch => "Searched",
            GroupKind::Dir => "Listed",
            GroupKind::WebFetch => "Fetched",
        }
    }

    /// Noun, pluralized when `count != 1`.
    pub fn noun(self, count: usize) -> &'static str {
        match (self, count) {
            (GroupKind::File, 1) => "file",
            (GroupKind::File, _) => "files",
            (GroupKind::Skill, 1) => "skill",
            (GroupKind::Skill, _) => "skills",
            (GroupKind::Search, 1) => "pattern",
            (GroupKind::Search, _) => "patterns",
            (GroupKind::Dir, 1) => "dir",
            (GroupKind::Dir, _) => "dirs",
            (GroupKind::WebFetch | GroupKind::WebSearch, 1) => "website",
            (GroupKind::WebFetch | GroupKind::WebSearch, _) => "websites",
        }
    }
}

/// A tool result: the full text (capped at [`TOOL_RESULT_MAX_BYTES`]) plus
/// its error flag.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// Full result text, cut at [`TOOL_RESULT_MAX_BYTES`] on a char boundary.
    pub text: String,
    /// True when `text` was cut at the storage cap.
    pub truncated: bool,
    pub is_error: bool,
}

impl ToolOutcome {
    fn new(text: String, is_error: bool) -> Self {
        let truncated = text.len() > TOOL_RESULT_MAX_BYTES;
        let text = if truncated {
            let mut cut = TOOL_RESULT_MAX_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_string()
        } else {
            text
        };
        ToolOutcome {
            text,
            truncated,
            is_error,
        }
    }

    /// Collapsed display line: the whole text clipped at 160 chars, first
    /// line only (the legacy stream's derivation, now an accessor).
    pub fn first_line(&self) -> String {
        _clip(&self.text, 160)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    pub kind: GroupKind,
    /// Raw tool name (`Read`, `Grep`, …).
    pub name: String,
    /// Most relevant input field (path, pattern, url, query).
    pub hint: String,
    /// Pretty-printed full input JSON, for the block viewer.
    pub input_json: String,
    pub result: Option<ToolOutcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolGroupBlock {
    /// Members in arrival order (same kind accumulates into one bucket).
    pub members: Vec<GroupMember>,
}

impl ToolGroupBlock {
    /// Buckets `(kind, count)` in first-appearance order.
    pub fn buckets(&self) -> Vec<(GroupKind, usize)> {
        let mut buckets: Vec<(GroupKind, usize)> = Vec::new();
        for member in &self.members {
            match buckets.iter_mut().find(|(k, _)| *k == member.kind) {
                Some((_, n)) => *n += 1,
                None => buckets.push((member.kind, 1)),
            }
        }
        buckets
    }

    /// `Read 1 file, Searched 2 patterns` — the whole aggregate label.
    pub fn label(&self) -> String {
        self.buckets()
            .into_iter()
            .map(|(kind, n)| format!("{} {} {}", kind.verb(), n, kind.noun(n)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Members whose result came back `is_error` (drives ` · N failed`).
    pub fn failed(&self) -> usize {
        self.members
            .iter()
            .filter(|m| m.result.as_ref().is_some_and(|r| r.is_error))
            .count()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunBlock {
    /// Display text after `Run `: the description with newlines flattened and
    /// a leading `Run`/`Running` word stripped, else the command's first
    /// line, else `…`.
    pub description: String,
    /// Full command text (all lines), for the block viewer; empty when the
    /// input carried none.
    pub command: String,
    pub result: Option<ToolOutcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolBlock {
    pub name: String,
    pub hint: String,
    /// Pretty-printed full input JSON, for the block viewer.
    pub input_json: String,
    pub result: Option<ToolOutcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingBlock {
    /// Full thinking text from the transcript's thinking content block.
    pub text: String,
    /// Seconds between the previous transcript row and the thinking row;
    /// `None` when either timestamp is missing.
    pub duration_secs: Option<f64>,
}

impl ThinkingBlock {
    /// `Thought for 14.3s`, or bare `Thought` without a duration.
    pub fn label(&self) -> String {
        match self.duration_secs {
            Some(secs) => format!("Thought for {}", format_thinking_duration(secs)),
            None => "Thought".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantBlock {
    /// Raw markdown source; render with [`grok_md`].
    pub markdown: String,
    pub timestamp: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkedForBlock {
    /// User-message timestamp → final assistant text timestamp of the turn.
    pub duration_secs: Option<f64>,
}

impl WorkedForBlock {
    /// `Worked for 4m6s`, or `Turn completed.` without a duration.
    pub fn label(&self) -> String {
        match self.duration_secs {
            Some(secs) => format!("Worked for {}", format_worked_duration(secs)),
            None => "Turn completed.".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

enum Pending {
    Group {
        entry_id: u64,
        block: ToolGroupBlock,
        ids: Vec<String>,
    },
    Run {
        entry_id: u64,
        block: RunBlock,
        id: String,
    },
    Tool {
        entry_id: u64,
        block: ToolBlock,
        id: String,
    },
}

impl Pending {
    fn into_entry(self) -> Entry {
        match self {
            Pending::Group {
                entry_id, block, ..
            } => Entry {
                id: entry_id,
                block: DisplayBlock::ToolGroup(block),
            },
            Pending::Run {
                entry_id, block, ..
            } => Entry {
                id: entry_id,
                block: DisplayBlock::Run(block),
            },
            Pending::Tool {
                entry_id, block, ..
            } => Entry {
                id: entry_id,
                block: DisplayBlock::Tool(block),
            },
        }
    }

    fn snapshot(&self) -> Entry {
        match self {
            Pending::Group {
                entry_id, block, ..
            } => Entry {
                id: *entry_id,
                block: DisplayBlock::ToolGroup(block.clone()),
            },
            Pending::Run {
                entry_id, block, ..
            } => Entry {
                id: *entry_id,
                block: DisplayBlock::Run(block.clone()),
            },
            Pending::Tool {
                entry_id, block, ..
            } => Entry {
                id: *entry_id,
                block: DisplayBlock::Tool(block.clone()),
            },
        }
    }

    /// Complete once its result attached; open groups never self-complete.
    fn is_complete(&self) -> bool {
        match self {
            Pending::Group { .. } => false,
            Pending::Run { block, .. } => block.result.is_some(),
            Pending::Tool { block, .. } => block.result.is_some(),
        }
    }
}

/// Which aggregation bucket a tool_use joins, or `None` for non-members.
fn member_kind(name: &str, input: &Value) -> Option<GroupKind> {
    Some(match name {
        "Read" => {
            let path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
            if path.ends_with("SKILL.md") {
                GroupKind::Skill
            } else {
                GroupKind::File
            }
        }
        "Grep" | "Glob" => GroupKind::Search,
        "LS" => GroupKind::Dir,
        "WebFetch" => GroupKind::WebFetch,
        "WebSearch" => GroupKind::WebSearch,
        "Skill" => GroupKind::Skill,
        _ => return None,
    })
}

fn generic_hint(input: &Value) -> String {
    let hint = ["description", "file_path", "command", "prompt"]
        .into_iter()
        .find_map(|k| {
            input
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| serde_json::to_string(input).unwrap_or_default());
    hint.lines().next().unwrap_or("").to_string()
}

/// Pretty-printed full input JSON, stored for the block viewer.
fn input_json(input: &Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_default()
}

fn member_hint(name: &str, input: &Value) -> String {
    let key = match name {
        "Read" => "file_path",
        "Grep" | "Glob" => "pattern",
        "LS" => "path",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Skill" => "skill",
        _ => "",
    };
    if let Some(v) = input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return v.lines().next().unwrap_or("").to_string();
    }
    generic_hint(input)
}

/// Bash display text per grok execute.rs: description trimmed, newlines
/// folded to spaces, a leading `Run`/`Running` word stripped; fallback is the
/// command's first line; an empty command renders `…`.
fn run_description(input: &Value) -> String {
    if let Some(desc) = input.get("description").and_then(Value::as_str) {
        let flat = desc.split_whitespace().collect::<Vec<_>>().join(" ");
        let stripped = flat
            .strip_prefix("Running ")
            .or_else(|| flat.strip_prefix("Run "))
            .unwrap_or(&flat)
            .trim();
        if !stripped.is_empty() {
            return stripped.to_string();
        }
        if !flat.is_empty() {
            return flat;
        }
    }
    if let Some(cmd) = input.get("command").and_then(Value::as_str) {
        let first = cmd.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    "…".to_string()
}

/// Fold transcript JSONL lines into [`DisplayBlock`]s, streaming.
///
/// `push_entries` returns the blocks *finalized* by that line — with their
/// stable ids — in display order; `pending_entries` snapshots what is still
/// open (an aggregating tool group, a running Bash) for live rendering;
/// `flush_entries` force-finalizes the rest. `push`/`pending_blocks`/`flush`
/// are the id-less views over the same stream.
pub struct TranscriptParser {
    pending: Vec<Pending>,
    next_id: u64,
    prev_row_ms: Option<i64>,
    turn_start_ms: Option<i64>,
    last_assistant_text_ms: Option<i64>,
    turn_has_assistant_text: bool,
    tokens: i64,
    busy: bool,
    model: Option<String>,
    effort: Option<String>,
}

impl Default for TranscriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptParser {
    pub fn new() -> Self {
        TranscriptParser {
            pending: Vec::new(),
            next_id: 0,
            prev_row_ms: None,
            turn_start_ms: None,
            last_assistant_text_ms: None,
            turn_has_assistant_text: false,
            tokens: 0,
            busy: false,
            model: None,
            effort: None,
        }
    }

    /// Mint the next block identity; each block takes exactly one, at birth.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Accumulated `output_tokens` across all rows seen.
    pub fn output_tokens(&self) -> i64 {
        self.tokens
    }

    /// True between a user message / tool_use and the next assistant text.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Model id from the most recent assistant row, if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }


    /// Elapsed seconds of the still-open turn once it has settled (idle with
    /// both a turn start and a final assistant text). The real WorkedFor
    /// block is only emitted when the NEXT user message closes the turn —
    /// this lets a live view synthesize the line in the meantime.
    pub fn open_turn_worked_secs(&self) -> Option<f64> {
        if self.busy {
            return None;
        }
        match (self.turn_start_ms, self.last_assistant_text_ms) {
            (Some(start), Some(end)) if end >= start => Some((end - start) as f64 / 1000.0),
            _ => None,
        }
    }

    /// Reasoning-effort level from the most recent row carrying one.
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Snapshot of the not-yet-finalized tail, in display order. Ids are
    /// stable across snapshots and survive into the finalized stream.
    pub fn pending_entries(&self) -> Vec<Entry> {
        self.pending.iter().map(Pending::snapshot).collect()
    }

    /// Snapshot of the not-yet-finalized tail, blocks only.
    pub fn pending_blocks(&self) -> Vec<DisplayBlock> {
        self.pending.iter().map(|p| p.snapshot().block).collect()
    }

    /// Force-finalize everything pending (EOF, idle flush).
    pub fn flush_entries(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        self.drain_all(&mut out);
        out
    }

    /// Force-finalize everything pending, blocks only.
    pub fn flush(&mut self) -> Vec<DisplayBlock> {
        self.flush_entries().into_iter().map(|e| e.block).collect()
    }

    fn drain_all(&mut self, out: &mut Vec<Entry>) {
        for item in self.pending.drain(..) {
            out.push(item.into_entry());
        }
    }

    /// Emit the settled prefix of `pending`: complete runs/tools, and groups
    /// that stopped aggregating because a non-member was queued after them.
    /// Stops at the first incomplete item so display order is preserved.
    fn drain_settled(&mut self, out: &mut Vec<Entry>) {
        loop {
            let deliverable = match self.pending.first() {
                Some(Pending::Group { .. }) => self.pending.len() > 1,
                Some(item) => item.is_complete(),
                None => false,
            };
            if !deliverable {
                break;
            }
            out.push(self.pending.remove(0).into_entry());
        }
    }

    fn attach_result(&mut self, id: &str, outcome: ToolOutcome) {
        if id.is_empty() {
            return;
        }
        for item in &mut self.pending {
            match item {
                Pending::Group { block, ids, .. } => {
                    if let Some(i) = ids.iter().position(|x| x == id) {
                        block.members[i].result = Some(outcome);
                        return;
                    }
                }
                Pending::Run { block, id: rid, .. } => {
                    if rid == id {
                        block.result = Some(outcome);
                        return;
                    }
                }
                Pending::Tool { block, id: rid, .. } => {
                    if rid == id {
                        block.result = Some(outcome);
                        return;
                    }
                }
            }
        }
    }

    /// Feed one raw JSONL line; returns the blocks it finalized, in order.
    pub fn push(&mut self, raw: &str) -> Vec<DisplayBlock> {
        self.push_entries(raw)
            .into_iter()
            .map(|e| e.block)
            .collect()
    }

    /// Feed one raw JSONL line; returns the entries it finalized, in order.
    pub fn push_entries(&mut self, raw: &str) -> Vec<Entry> {
        let mut out = Vec::new();
        let row: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return out,
        };
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            return out;
        }
        let is_user = kind == "user";
        let ts = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        let null = Value::Null;
        let message = row.get("message").unwrap_or(&null);
        if let Some(m) = message.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        if let Some(e) = row.get("effort").and_then(Value::as_str) {
            self.effort = Some(e.to_string());
        }
        let usage = message.get("usage").unwrap_or(&null);
        self.tokens += usage
            .get("output_tokens")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0);
        let content = message.get("content").unwrap_or(&null);
        let blocks: Vec<Value> = if let Some(s) = content.as_str() {
            vec![serde_json::json!({"type": "text", "text": s})]
        } else if let Some(arr) = content.as_array() {
            arr.clone()
        } else {
            Vec::new()
        };
        // Anchor for thinking durations: the previous row's instant; a second
        // thinking block in the same row measures from this row instead.
        let mut thinking_anchor_ms = self.prev_row_ms;
        for block in &blocks {
            if !block.is_object() {
                continue;
            }
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let body = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if body.is_empty() {
                        continue;
                    }
                    self.drain_all(&mut out);
                    if is_user {
                        if self.turn_has_assistant_text {
                            let duration_secs =
                                match (self.turn_start_ms, self.last_assistant_text_ms) {
                                    (Some(start), Some(end)) if end >= start => {
                                        Some((end - start) as f64 / 1000.0)
                                    }
                                    _ => None,
                                };
                            out.push(Entry {
                                id: self.alloc_id(),
                                block: DisplayBlock::WorkedFor(WorkedForBlock { duration_secs }),
                            });
                        }
                        self.turn_start_ms = ts.as_ref().map(|t| t.epoch_ms);
                        self.last_assistant_text_ms = None;
                        self.turn_has_assistant_text = false;
                        out.push(Entry {
                            id: self.alloc_id(),
                            block: DisplayBlock::User(UserBlock {
                                text: body.to_string(),
                                hive: parse_hive_message(body),
                                timestamp: ts.clone(),
                            }),
                        });
                        self.busy = true;
                    } else {
                        out.push(Entry {
                            id: self.alloc_id(),
                            block: DisplayBlock::Assistant(AssistantBlock {
                                markdown: body.to_string(),
                                timestamp: ts.clone(),
                            }),
                        });
                        self.last_assistant_text_ms = ts.as_ref().map(|t| t.epoch_ms);
                        self.turn_has_assistant_text = true;
                        self.busy = false;
                    }
                }
                "thinking" if !is_user => {
                    self.drain_all(&mut out);
                    let duration_secs = match (thinking_anchor_ms, ts.as_ref()) {
                        (Some(prev), Some(t)) if t.epoch_ms >= prev => {
                            Some((t.epoch_ms - prev) as f64 / 1000.0)
                        }
                        _ => None,
                    };
                    let text = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out.push(Entry {
                        id: self.alloc_id(),
                        block: DisplayBlock::Thinking(ThinkingBlock {
                            text,
                            duration_secs,
                        }),
                    });
                    if let Some(t) = ts.as_ref() {
                        thinking_anchor_ms = Some(t.epoch_ms);
                    }
                }
                "tool_use" if !is_user => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let empty_obj = Value::Object(serde_json::Map::new());
                    let input = match block.get("input") {
                        Some(v @ Value::Object(_)) => v,
                        _ => &empty_obj,
                    };
                    self.busy = true;
                    if let Some(kind) = member_kind(name, input) {
                        let member = GroupMember {
                            kind,
                            name: name.to_string(),
                            hint: member_hint(name, input),
                            input_json: input_json(input),
                            result: None,
                        };
                        match self.pending.last_mut() {
                            Some(Pending::Group { block, ids, .. }) => {
                                block.members.push(member);
                                ids.push(id);
                            }
                            _ => {
                                let entry_id = self.alloc_id();
                                self.pending.push(Pending::Group {
                                    entry_id,
                                    block: ToolGroupBlock {
                                        members: vec![member],
                                    },
                                    ids: vec![id],
                                });
                            }
                        }
                    } else if name == "Bash" {
                        let entry_id = self.alloc_id();
                        self.pending.push(Pending::Run {
                            entry_id,
                            block: RunBlock {
                                description: run_description(input),
                                command: input
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                result: None,
                            },
                            id,
                        });
                    } else {
                        let entry_id = self.alloc_id();
                        self.pending.push(Pending::Tool {
                            entry_id,
                            block: ToolBlock {
                                name: name.to_string(),
                                hint: generic_hint(input),
                                input_json: input_json(input),
                                result: None,
                            },
                            id,
                        });
                    }
                    self.drain_settled(&mut out);
                }
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let body = block.get("content").unwrap_or(&null);
                    let text = match body.as_str() {
                        Some(s) => s.to_string(),
                        None => serde_json::to_string(body).unwrap_or_default(),
                    };
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let outcome = ToolOutcome::new(text, is_error);
                    self.attach_result(id, outcome);
                    self.drain_settled(&mut out);
                }
                _ => {}
            }
        }
        if let Some(t) = ts.as_ref() {
            self.prev_row_ms = Some(t.epoch_ms);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Plain (non-tty) ANSI stream over the block model
// ---------------------------------------------------------------------------

fn _tool_line(name: &str, hint: &str) -> String {
    let hint = _clip(hint.lines().next().unwrap_or(""), 140);
    format!("{GREEN}⏺{RESET} {BOLD}{name}{RESET}({CYAN}{hint}{RESET})")
}

fn _result_line(result: &Option<ToolOutcome>) -> Option<String> {
    let res = result.as_ref()?;
    let first = res.first_line();
    let first = first.trim();
    if first.is_empty() {
        return None;
    }
    Some(format!("\n  {DIM}⎿  {first}{RESET}"))
}

fn _user_line(text: &str) -> String {
    if let Some(msg) = parse_hive_message(text) {
        let sender = msg.from.as_deref().unwrap_or("peer");
        let body = _clip(&msg.body, 160);
        return format!("{MAGENTA}✉{RESET} {BOLD}{sender}{RESET} {DIM}▸{RESET} {body}");
    }
    let first = format!("{BOLD}❯{RESET} {BOLD}");
    format!(
        "{}{}",
        _indent_block(&_clip(text, 1200), &first, "  "),
        RESET
    )
}

/// Print [`DisplayBlock`]s as the legacy plain ANSI stream (piped mode).
struct StreamPrinter {
    parser: TranscriptParser,
    state: &'static str, // idle | working
    state_since: Instant,
}

impl StreamPrinter {
    fn new() -> Self {
        StreamPrinter {
            parser: TranscriptParser::new(),
            state: "idle",
            state_since: Instant::now(),
        }
    }

    fn sync_state(&mut self) {
        let state = if self.parser.busy() {
            "working"
        } else {
            "idle"
        };
        if state != self.state {
            self.state = state;
            self.state_since = Instant::now();
        }
    }

    fn push_rendered(&mut self, raw: &str) -> Option<String> {
        let blocks = self.parser.push(raw);
        self.sync_state();
        Self::render_blocks(&blocks)
    }

    fn flush_rendered(&mut self) -> Option<String> {
        let blocks = self.parser.flush();
        Self::render_blocks(&blocks)
    }

    fn render_blocks(blocks: &[DisplayBlock]) -> Option<String> {
        let mut out = String::new();
        for block in blocks {
            if let Some(s) = Self::render_block(block) {
                out.push_str(&s);
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn render_block(block: &DisplayBlock) -> Option<String> {
        match block {
            DisplayBlock::User(u) => Some(format!("\n{}", _user_line(&u.text))),
            DisplayBlock::Assistant(a) => Some(format!(
                "\n{}",
                _indent_block(&_md(&_clip(&a.markdown, 4000)), "⏺ ", "  ")
            )),
            DisplayBlock::ToolGroup(group) => {
                let mut out = String::new();
                for member in &group.members {
                    out.push('\n');
                    out.push_str(&_tool_line(&member.name, &member.hint));
                    if let Some(res) = _result_line(&member.result) {
                        out.push_str(&res);
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
            DisplayBlock::Run(run) => {
                let mut out = format!("\n{}", _tool_line("Bash", &run.description));
                if let Some(res) = _result_line(&run.result) {
                    out.push_str(&res);
                }
                Some(out)
            }
            DisplayBlock::Tool(tool) => {
                let mut out = format!("\n{}", _tool_line(&tool.name, &tool.hint));
                if let Some(res) = _result_line(&tool.result) {
                    out.push_str(&res);
                }
                Some(out)
            }
            // The plain stream never showed thinking or turn markers.
            DisplayBlock::Thinking(_) | DisplayBlock::WorkedFor(_) => None,
        }
    }

    fn status_line(&self, tick: usize, session_id: &str) -> String {
        let verb = if self.state == "working" {
            let frames: Vec<char> = _SPINNER.chars().collect();
            let frame = frames[tick % frames.len()];
            let elapsed = self.state_since.elapsed().as_secs();
            format!("{YELLOW}{frame}{RESET} Working… {DIM}({elapsed}s){RESET}")
        } else {
            format!("{GREEN}●{RESET} idle")
        };
        let sid: String = session_id.chars().take(8).collect();
        format!(
            "{verb} {DIM}· {sid} · {} tokens out · read-only mirror{RESET}",
            self.parser.output_tokens()
        )
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn follow(session_id: &str) -> i32 {
    let path = match transcript_path(session_id) {
        Some(p) => p,
        None => {
            println!("no transcript for session '{}'", session_id);
            return 1;
        }
    };
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
        return match crate::transcript_tui::run(&path) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("{}: {}", path.display(), err);
                1
            }
        };
    }
    follow_plain(session_id, &path)
}

/// Legacy plain ANSI stream, used when stdout is not a tty (pipes, logs).
fn follow_plain(session_id: &str, path: &Path) -> i32 {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("{DIM}── live mirror · {name} · keys go nowhere ──{RESET}");
    let mut printer = StreamPrinter::new();
    let mut tick: usize = 0;
    let mut idle_ticks: usize = 0;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("{}: {}", path.display(), err);
            return 1;
        }
    };
    let mut reader = BufReader::new(file);
    let mut backlog = String::new();
    if let Err(err) = reader.read_to_string(&mut backlog) {
        eprintln!("{}: {}", path.display(), err);
        return 1;
    }
    let lines: Vec<&str> = backlog.lines().collect();
    for raw in &lines[lines.len().saturating_sub(_TAIL_EVENTS)..] {
        if let Some(rendered) = printer.push_rendered(raw) {
            println!("{rendered}");
        }
    }
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let mut raw = String::new();
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            print!("{CLEAR_LINE}");
            let _ = std::io::stdout().flush();
            return 0;
        }
        raw.clear();
        match reader.read_line(&mut raw) {
            Ok(0) => {
                tick += 1;
                idle_ticks += 1;
                if idle_ticks == _IDLE_FLUSH_TICKS {
                    if let Some(rendered) = printer.flush_rendered() {
                        print!("{CLEAR_LINE}");
                        println!("{rendered}");
                    }
                }
                print!("{}{}", CLEAR_LINE, printer.status_line(tick, session_id));
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs_f64(_POLL_SECONDS));
            }
            Ok(_) => {
                idle_ticks = 0;
                if let Some(rendered) = printer.push_rendered(&raw) {
                    print!("{CLEAR_LINE}");
                    println!("{rendered}");
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => {
                eprintln!("{}: {}", path.display(), err);
                return 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn _row_at(kind: &str, content: Value, usage: Option<Value>, ts: Option<&str>) -> String {
        let mut msg = json!({ "content": content });
        if let Some(u) = usage {
            msg["usage"] = u;
        }
        let mut row = json!({ "type": kind, "message": msg });
        if let Some(t) = ts {
            row["timestamp"] = json!(t);
        }
        row.to_string()
    }

    fn _row(kind: &str, content: Value, usage: Option<Value>) -> String {
        _row_at(kind, content, usage, None)
    }

    fn _tool_use(name: &str, id: &str, input: Value) -> String {
        _row(
            "assistant",
            json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
            None,
        )
    }

    fn _tool_result(id: &str, content: Value, is_error: bool) -> String {
        _row(
            "user",
            json!([{"type": "tool_result", "tool_use_id": id,
                    "content": content, "is_error": is_error}]),
            None,
        )
    }

    fn _text(kind: &str, body: &str) -> String {
        _row(kind, json!([{"type": "text", "text": body}]), None)
    }

    // ---- ported plain-stream tests -------------------------------------

    #[test]
    fn test_assistant_text_renders_with_marker_and_markdown() {
        let mut p = StreamPrinter::new();
        let out = p
            .push_rendered(&_text("assistant", "done: **all green**"))
            .unwrap();
        assert!(out.contains("⏺"), "{out}");
        // grok markdown engine: bold content survives, markers are hidden
        assert!(out.contains("all green"), "{out}");
        assert!(!out.contains("**"), "{out}");
        assert_eq!(p.state, "idle");
    }

    #[test]
    fn test_tool_use_prefers_the_human_readable_hint() {
        let mut p = StreamPrinter::new();
        let pushed = p.push_rendered(&_tool_use(
            "Bash",
            "t1",
            json!({"command": "ls", "description": "List files"}),
        ));
        assert!(pushed.is_none(), "runs finalize late");
        assert_eq!(p.state, "working");
        let out = p.flush_rendered().unwrap();
        assert!(out.contains("Bash") && out.contains("List files"));
        assert!(!out.replace("List files", "").contains("ls"));
    }

    #[test]
    fn test_parse_hive_message_reads_every_arrival_shape() {
        // bare: typed straight into the pane.
        let bare = parse_hive_message(
            "<HIVE from=comb.dodo to=comb.rex msgId=a1 reply-to=z9 artifact=/tmp/spec.md>\nreview the spec\n</HIVE>",
        )
        .unwrap();
        assert_eq!(bare.from.as_deref(), Some("comb.dodo"));
        assert_eq!(bare.to.as_deref(), Some("comb.rex"));
        assert_eq!(bare.msg_id.as_deref(), Some("a1"));
        assert_eq!(bare.reply_to.as_deref(), Some("z9"));
        assert_eq!(bare.artifact.as_deref(), Some("/tmp/spec.md"));
        assert_eq!(bare.body, "review the spec");
        assert!(!bare.injected && !bare.mid_turn);

        // claude's session-inbox injection, turn start.
        let turn_start = parse_hive_message(
            "Another Claude session sent a message:\n\
             <HIVE from=sage to=orch msgId=7boK>\ndone\n</HIVE>\n\n\
             This came from another Claude session — not typed by your user, but very \
             likely working on their behalf. …permission laundering.",
        )
        .unwrap();
        assert_eq!(turn_start.from.as_deref(), Some("sage"));
        assert_eq!(turn_start.body, "done");
        assert!(turn_start.injected && !turn_start.mid_turn);

        // same wrapper, folded into a turn already in flight.
        let mid = parse_hive_message(
            "Another Claude session sent a message while you were working:\n\
             <HIVE from=sage to=orch>\ndone\n</HIVE>\n\n\
             This came from another Claude session — …",
        )
        .unwrap();
        assert!(mid.injected && mid.mid_turn);

        // the retired <channel> transport, still in old transcripts.
        let chan = parse_hive_message(
            "<channel source=\"plugin:hive-channel:hive-channel\" msg_id=\"18kd\">\n\
             <HIVE from=validator to=worker msgId=18kd>\nrt-1785757288\n</HIVE>\n</channel>",
        )
        .unwrap();
        assert_eq!(chan.from.as_deref(), Some("validator"));
        assert_eq!(chan.body, "rt-1785757288");

        // attribute-less envelope still parses; the body is what matters.
        let bald = parse_hive_message("<HIVE>hi</HIVE>").unwrap();
        assert_eq!(bald.from, None);
        assert_eq!(bald.body, "hi");
    }

    #[test]
    fn test_parse_hive_message_ignores_prose_that_quotes_an_envelope() {
        // skill docs and specs quote the envelope; they are not messages.
        assert!(parse_hive_message(
            "其他 agent 的消息会以 `<HIVE from=a to=b>body</HIVE>` 注入当前 pane。"
        )
        .is_none());
        assert!(parse_hive_message("<HIVE from=a to=b>unterminated").is_none());
        assert!(parse_hive_message("<HIVEISH from=a>x</HIVE>").is_none());
        // a body that merely mentions the tag stays whole.
        let msg = parse_hive_message("<HIVE from=probe to=kilo>你上下文里的 <HIVE> 消息</HIVE>")
            .unwrap();
        assert_eq!(msg.body, "你上下文里的 <HIVE> 消息");
    }

    #[test]
    fn test_hive_envelope_collapses_to_a_tagged_line() {
        let mut p = StreamPrinter::new();
        let body = "<HIVE from=comb.dodo to=comb.rex msgId=a1>review the spec</HIVE>";
        let out = p.push_rendered(&_row("user", json!(body), None)).unwrap();
        assert!(out.contains("✉") && out.contains("comb.dodo") && out.contains("review the spec"));
        assert!(!out.contains("<HIVE"));
        assert_eq!(p.state, "working");
    }

    #[test]
    fn test_user_turn_flips_working_and_final_text_flips_idle() {
        let mut p = StreamPrinter::new();
        p.push_rendered(&_row("user", json!("hi"), None));
        assert_eq!(p.state, "working");
        p.push_rendered(&_text("assistant", "hello"));
        assert_eq!(p.state, "idle");
    }

    #[test]
    fn test_output_tokens_accumulate_into_the_status_line() {
        let mut p = StreamPrinter::new();
        p.push_rendered(&_row(
            "assistant",
            json!([{"type": "text", "text": "a"}]),
            Some(json!({"output_tokens": 40})),
        ));
        p.push_rendered(&_row(
            "assistant",
            json!([{"type": "text", "text": "b"}]),
            Some(json!({"output_tokens": 2})),
        ));
        assert!(p.status_line(0, "deadbeef-1234").contains("42 tokens out"));
        assert_eq!(p.parser.output_tokens(), 42);
    }

    #[test]
    fn test_non_message_rows_render_nothing() {
        let mut p = StreamPrinter::new();
        assert!(p
            .push_rendered(&json!({"type": "system"}).to_string())
            .is_none());
        assert!(p.push_rendered("not json").is_none());
        assert!(p.parser.pending_blocks().is_empty());
    }

    // ---- parser: tool-group aggregation --------------------------------

    fn group(block: &DisplayBlock) -> &ToolGroupBlock {
        match block {
            DisplayBlock::ToolGroup(g) => g,
            other => panic!("expected ToolGroup, got {other:?}"),
        }
    }

    #[test]
    fn test_consecutive_read_tools_collapse_into_one_group() {
        let mut p = TranscriptParser::new();
        assert!(p
            .push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})))
            .is_empty());
        assert!(p
            .push(&_tool_use("Grep", "t2", json!({"pattern": "fn main"})))
            .is_empty());
        let out = p.push(&_text("assistant", "done"));
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(group(&out[0]).label(), "Read 1 file, Searched 1 pattern");
        assert!(matches!(out[1], DisplayBlock::Assistant(_)));
    }

    #[test]
    fn test_group_label_pluralizes_bucket_counts() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        p.push(&_tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
        p.push(&_tool_use("Glob", "t3", json!({"pattern": "*.rs"})));
        let out = p.flush();
        assert_eq!(group(&out[0]).label(), "Read 2 files, Searched 1 pattern");
    }

    #[test]
    fn test_group_bucket_order_follows_first_appearance() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Grep", "t1", json!({"pattern": "x"})));
        p.push(&_tool_use("Read", "t2", json!({"file_path": "/a.rs"})));
        let out = p.flush();
        assert_eq!(group(&out[0]).label(), "Searched 1 pattern, Read 1 file");
    }

    #[test]
    fn test_group_closes_when_a_bash_tool_arrives() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        let out = p.push(&_row(
            "assistant",
            json!([{"type": "tool_use", "id": "t2", "name": "Bash",
                    "input": {"command": "cargo build"}}]),
            None,
        ));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(group(&out[0]).label(), "Read 1 file");
        let pending = p.pending_blocks();
        assert!(
            matches!(&pending[..], [DisplayBlock::Run(_)]),
            "{pending:?}"
        );
    }

    #[test]
    fn test_tool_results_do_not_break_an_open_group() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        assert!(p
            .push(&_tool_result("t1", json!("1\tfn a"), false))
            .is_empty());
        p.push(&_tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
        let out = p.push(&_text("assistant", "done"));
        let g = group(&out[0]);
        assert_eq!(g.members.len(), 2);
        assert_eq!(g.label(), "Read 2 files");
        assert_eq!(
            g.members[0].result.as_ref().unwrap().first_line(),
            "1\tfn a"
        );
    }

    #[test]
    fn test_group_counts_failed_members() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        p.push(&_tool_use("Read", "t2", json!({"file_path": "/gone.rs"})));
        p.push(&_tool_result("t2", json!("no such file"), true));
        let out = p.flush();
        let g = group(&out[0]);
        assert_eq!(g.failed(), 1);
        assert!(g.members[1].result.as_ref().unwrap().is_error);
    }

    #[test]
    fn test_skill_read_buckets_as_skill() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use(
            "Read",
            "t1",
            json!({"file_path": "/plugins/x/skills/y/SKILL.md"}),
        ));
        let out = p.flush();
        assert_eq!(group(&out[0]).label(), "Read 1 skill");
    }

    // ---- parser: bash run wording --------------------------------------

    fn run(block: &DisplayBlock) -> &RunBlock {
        match block {
            DisplayBlock::Run(r) => r,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn test_bash_description_strips_run_prefix_and_newlines() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use(
            "Bash",
            "t1",
            json!({"command": "cargo test", "description": "Run the\ntests"}),
        ));
        let out = p.flush();
        assert_eq!(run(&out[0]).description, "the tests");
    }

    #[test]
    fn test_bash_falls_back_to_command_first_line() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Bash", "t1", json!({"command": "ls -la\npwd"})));
        p.push(&_tool_use("Bash", "t2", json!({})));
        let out = p.flush();
        assert_eq!(run(&out[0]).description, "ls -la");
        assert_eq!(run(&out[1]).description, "…");
    }

    #[test]
    fn test_run_finalizes_when_its_result_attaches() {
        let mut p = TranscriptParser::new();
        assert!(p
            .push(&_tool_use("Bash", "t1", json!({"command": "cargo build"})))
            .is_empty());
        let out = p.push(&_tool_result("t1", json!("Compiling hive"), false));
        assert_eq!(out.len(), 1, "{out:?}");
        let r = run(&out[0]);
        assert_eq!(r.result.as_ref().unwrap().first_line(), "Compiling hive");
        assert!(p.pending_blocks().is_empty());
    }

    #[test]
    fn test_other_tools_keep_name_and_hint() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Edit", "t1", json!({"file_path": "/a.rs"})));
        let out = p.flush();
        match &out[0] {
            DisplayBlock::Tool(t) => {
                assert_eq!(t.name, "Edit");
                assert_eq!(t.hint, "/a.rs");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    // ---- parser: thinking ----------------------------------------------

    #[test]
    fn test_thinking_duration_comes_from_adjacent_row_timestamps() {
        let mut p = TranscriptParser::new();
        p.push(&_row_at(
            "user",
            json!("go"),
            None,
            Some("2026-08-30T12:40:00.000Z"),
        ));
        let out = p.push(&_row_at(
            "assistant",
            json!([{"type": "thinking", "thinking": "hmm"}]),
            None,
            Some("2026-08-30T12:40:14.300Z"),
        ));
        match &out[0] {
            DisplayBlock::Thinking(t) => {
                assert_eq!(t.duration_secs, Some(14.3));
                assert_eq!(t.label(), "Thought for 14.3s");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn test_thinking_without_timestamps_has_no_duration() {
        let mut p = TranscriptParser::new();
        let out = p.push(&_row(
            "assistant",
            json!([{"type": "thinking", "thinking": "hmm"}]),
            None,
        ));
        match &out[0] {
            DisplayBlock::Thinking(t) => {
                assert_eq!(t.duration_secs, None);
                assert_eq!(t.label(), "Thought");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn test_thinking_breaks_an_open_tool_group() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        let out = p.push(&_row(
            "assistant",
            json!([{"type": "thinking", "thinking": "hmm"}]),
            None,
        ));
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(group(&out[0]).label(), "Read 1 file");
        assert!(matches!(out[1], DisplayBlock::Thinking(_)));
    }

    // ---- parser: worked-for --------------------------------------------

    #[test]
    fn test_worked_for_spans_user_msg_to_final_assistant_text() {
        let mut p = TranscriptParser::new();
        p.push(&_row_at(
            "user",
            json!("go"),
            None,
            Some("2026-08-30T12:40:00.000Z"),
        ));
        p.push(&_row_at(
            "assistant",
            json!([{"type": "text", "text": "done"}]),
            None,
            Some("2026-08-30T12:44:06.000Z"),
        ));
        let out = p.push(&_row_at(
            "user",
            json!("next"),
            None,
            Some("2026-08-30T12:50:00.000Z"),
        ));
        assert_eq!(out.len(), 2, "{out:?}");
        match &out[0] {
            DisplayBlock::WorkedFor(w) => {
                assert_eq!(w.duration_secs, Some(246.0));
                assert_eq!(w.label(), "Worked for 4m6s");
            }
            other => panic!("expected WorkedFor, got {other:?}"),
        }
        assert!(matches!(out[1], DisplayBlock::User(_)));
    }

    #[test]
    fn test_worked_for_skipped_without_assistant_text() {
        let mut p = TranscriptParser::new();
        p.push(&_row("user", json!("go"), None));
        let out = p.push(&_row("user", json!("actually wait"), None));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0], DisplayBlock::User(_)));
    }

    // ---- durations & timestamps ----------------------------------------

    #[test]
    fn test_thinking_duration_formats() {
        assert_eq!(format_thinking_duration(0.84), "0.8s");
        assert_eq!(format_thinking_duration(14.3), "14.3s");
        assert_eq!(format_thinking_duration(72.4), "1m12s");
    }

    #[test]
    fn test_worked_for_duration_formats() {
        assert_eq!(format_worked_duration(4.42), "4.4s");
        assert_eq!(format_worked_duration(32.9), "32s");
        assert_eq!(format_worked_duration(246.0), "4m6s");
        assert_eq!(format_worked_duration(3720.0), "1h2m");
    }

    #[test]
    fn test_timestamps_render_local_clock() {
        std::env::set_var("TZ", "UTC");
        let epoch = parse_timestamp("1970-01-01T00:00:00.000Z").unwrap();
        assert_eq!(epoch.epoch_ms, 0);
        assert_eq!(epoch.clock, "12:00 AM");
        let noonish = parse_timestamp("2026-08-30T12:40:03.500Z").unwrap();
        assert_eq!(noonish.clock, "12:40 PM");
        let morning = parse_timestamp("2026-08-30T09:05:00Z").unwrap();
        assert_eq!(morning.clock, "9:05 AM");
    }

    #[test]
    fn test_user_and_assistant_blocks_carry_timestamps() {
        std::env::set_var("TZ", "UTC");
        let mut p = TranscriptParser::new();
        let out = p.push(&_row_at(
            "user",
            json!("hello"),
            None,
            Some("2026-08-30T12:40:00.000Z"),
        ));
        match &out[0] {
            DisplayBlock::User(u) => {
                assert!(u.hive.is_none());
                assert_eq!(u.timestamp.as_ref().unwrap().clock, "12:40 PM");
            }
            other => panic!("expected User, got {other:?}"),
        }
        let out = p.push(&_row_at(
            "assistant",
            json!([{"type": "text", "text": "hi"}]),
            None,
            Some("2026-08-30T12:41:00.000Z"),
        ));
        match &out[0] {
            DisplayBlock::Assistant(a) => {
                assert_eq!(a.markdown, "hi");
                assert_eq!(a.timestamp.as_ref().unwrap().clock, "12:41 PM");
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn test_pending_blocks_snapshot_shows_open_group() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        p.push(&_tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
        let pending = p.pending_blocks();
        assert_eq!(pending.len(), 1, "{pending:?}");
        let g = group(&pending[0]);
        assert_eq!(g.members.len(), 2);
        assert_eq!(g.label(), "Read 2 files");
        assert!(p.busy());
    }

    // ---- parser: full-content capture ----------------------------------

    #[test]
    fn test_thinking_block_captures_full_text() {
        let mut p = TranscriptParser::new();
        let out = p.push(&_row(
            "assistant",
            json!([{"type": "thinking", "thinking": "deep\nthought\nhere"}]),
            None,
        ));
        match &out[0] {
            DisplayBlock::Thinking(t) => assert_eq!(t.text, "deep\nthought\nhere"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn test_tool_outcome_stores_full_text_and_derives_first_line() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Bash", "t1", json!({"command": "cargo build"})));
        let out = p.push(&_tool_result(
            "t1",
            json!("Compiling hive\nFinished dev\nwarning: unused"),
            false,
        ));
        let res = run(&out[0]).result.as_ref().unwrap().clone();
        assert_eq!(res.text, "Compiling hive\nFinished dev\nwarning: unused");
        assert!(!res.truncated);
        assert_eq!(res.first_line(), "Compiling hive");
    }

    #[test]
    fn test_first_line_clips_long_lines_with_ellipsis() {
        let res = ToolOutcome::new("a".repeat(200), false);
        assert_eq!(res.first_line(), format!("{} …", "a".repeat(160)));
        assert!(!res.truncated, "200 chars is far below the storage cap");
        assert_eq!(res.text.len(), 200, "storage keeps the full text");
    }

    #[test]
    fn test_tool_outcome_truncates_at_byte_cap() {
        let res = ToolOutcome::new("x".repeat(TOOL_RESULT_MAX_BYTES + 1000), true);
        assert!(res.truncated);
        assert!(res.is_error);
        assert_eq!(res.text.len(), TOOL_RESULT_MAX_BYTES);
        let ok = ToolOutcome::new("x".repeat(TOOL_RESULT_MAX_BYTES), false);
        assert!(!ok.truncated, "exactly at the cap is not truncated");
        assert_eq!(ok.text.len(), TOOL_RESULT_MAX_BYTES);
    }

    #[test]
    fn test_tool_outcome_truncation_respects_char_boundaries() {
        // '宽' is 3 bytes; the cap is not a multiple of 3, so a naive byte
        // cut would split a char.
        let count = TOOL_RESULT_MAX_BYTES / 3 + 100;
        let res = ToolOutcome::new("宽".repeat(count), false);
        assert!(res.truncated);
        assert!(res.text.len() <= TOOL_RESULT_MAX_BYTES);
        assert!(res.text.chars().all(|c| c == '宽'), "no split chars");
    }

    #[test]
    fn test_run_block_keeps_full_command() {
        let mut p = TranscriptParser::new();
        p.push(&_tool_use(
            "Bash",
            "t1",
            json!({"command": "ls -la\npwd", "description": "List files"}),
        ));
        p.push(&_tool_use("Bash", "t2", json!({})));
        let out = p.flush();
        assert_eq!(run(&out[0]).command, "ls -la\npwd");
        assert_eq!(run(&out[1]).command, "", "absent command stores empty");
    }

    #[test]
    fn test_tool_block_keeps_full_input_json() {
        let input = json!({"file_path": "/a.rs", "old_string": "line1\nline2",
                           "new_string": "line3"});
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Edit", "t1", input.clone()));
        let out = p.flush();
        match &out[0] {
            DisplayBlock::Tool(t) => {
                let parsed: Value = serde_json::from_str(&t.input_json).unwrap();
                assert_eq!(parsed, input, "{}", t.input_json);
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn test_group_member_keeps_full_input_json() {
        let input = json!({"pattern": "fn main", "path": "/src", "-n": true});
        let mut p = TranscriptParser::new();
        p.push(&_tool_use("Grep", "t1", input.clone()));
        let out = p.flush();
        let g = group(&out[0]);
        let parsed: Value = serde_json::from_str(&g.members[0].input_json).unwrap();
        assert_eq!(parsed, input);
    }

    // ---- parser: entry identity & turns --------------------------------

    #[test]
    fn test_entry_ids_stable_from_pending_through_finalization() {
        let mut p = TranscriptParser::new();
        assert!(p
            .push_entries(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})))
            .is_empty());
        let snap = p.pending_entries();
        assert_eq!(snap.len(), 1);
        let group_id = snap[0].id;
        // Aggregating another member keeps the group's id.
        p.push_entries(&_tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
        assert_eq!(p.pending_entries()[0].id, group_id);
        // Finalization emits the same id, and later blocks mint higher ones.
        let out = p.push_entries(&_text("assistant", "done"));
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].id, group_id);
        assert!(matches!(out[0].block, DisplayBlock::ToolGroup(_)));
        assert!(out[1].id > group_id);
        assert!(matches!(out[1].block, DisplayBlock::Assistant(_)));
    }

    #[test]
    fn test_entry_ids_never_collide_between_pending_and_finalized() {
        let mut p = TranscriptParser::new();
        p.push_entries(&_tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
        let finalized = p.push_entries(&_row(
            "assistant",
            json!([{"type": "thinking", "thinking": "hmm"}]),
            None,
        ));
        p.push_entries(&_tool_use("Bash", "t2", json!({"command": "ls"})));
        let pending = p.pending_entries();
        let mut ids: Vec<u64> = finalized
            .iter()
            .chain(pending.iter())
            .map(|e| e.id)
            .collect();
        assert_eq!(
            ids,
            {
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                sorted
            },
            "ids are monotonic in display order"
        );
        ids.dedup();
        assert_eq!(ids.len(), 3, "group, thinking, run all distinct: {ids:?}");
        // The pending run finalizes with the id its snapshot showed.
        let out = p.flush_entries();
        assert_eq!(out[0].id, pending[0].id);
    }

    #[test]
    fn test_user_blocks_start_turns() {
        let mut p = TranscriptParser::new();
        let out = p.push_entries(&_row_at(
            "user",
            json!("go"),
            None,
            Some("2026-08-30T12:40:00.000Z"),
        ));
        assert!(out[0].block.starts_turn());
        let out = p.push_entries(&_row_at(
            "assistant",
            json!([{"type": "text", "text": "done"}]),
            None,
            Some("2026-08-30T12:41:00.000Z"),
        ));
        assert!(!out[0].block.starts_turn());
        // WorkedFor emitted ahead of the next user prompt is not a turn start.
        let out = p.push_entries(&_row_at(
            "user",
            json!("next"),
            None,
            Some("2026-08-30T12:42:00.000Z"),
        ));
        assert!(matches!(out[0].block, DisplayBlock::WorkedFor(_)));
        assert!(!out[0].block.starts_turn());
        assert!(out[1].block.starts_turn());
    }
}
