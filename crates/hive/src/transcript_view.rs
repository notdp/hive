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

    /// Render markdown to ratatui lines (the TUI mirror) in `theme`.
    pub fn render_ratatui(text: &str, theme: &ViewTheme) -> Vec<ratatui::text::Line<'static>> {
        let (lines, _) = xai_grok_markdown::render_markdown_ratatui(
            text,
            style(theme),
            true,
            Some(syntect(theme.kind)),
        );
        lines
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

/// `<HIVE\s+from=(\S+)[^>]*>\s*(.*?)\s*</HIVE>` with DOTALL, first match.
pub(crate) fn hive_envelope(text: &str) -> Option<(&str, &str)> {
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find("<HIVE") {
        let after_tag = search_from + rel + 5;
        let rest = &text[after_tag..];
        let ws_len = rest
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        if ws_len > 0 && rest[ws_len..].starts_with("from=") {
            let g1_start = after_tag + ws_len + 5;
            let run = &text[g1_start..];
            let run_len = run
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(run.len());
            if run_len > 0 {
                // `(\S+)` is greedy: try the longest prefix first, backtrack
                // until `[^>]*>` and a later `</HIVE>` can both match.
                let prefix = &run[..run_len];
                let mut ends: Vec<usize> = prefix.char_indices().map(|(i, _)| i).skip(1).collect();
                ends.push(run_len);
                for &p_len in ends.iter().rev() {
                    let after_g1 = g1_start + p_len;
                    if let Some(gt) = text[after_g1..].find('>') {
                        let body_start = after_g1 + gt + 1;
                        if let Some(close) = text[body_start..].find("</HIVE>") {
                            let sender = &text[g1_start..after_g1];
                            let body = text[body_start..body_start + close].trim();
                            return Some((sender, body));
                        }
                    }
                }
            }
        }
        search_from = search_from + rel + 1;
    }
    None
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

#[derive(Debug, Clone, PartialEq)]
pub struct UserBlock {
    /// Raw message text; for HIVE envelopes this still holds the full
    /// `<HIVE from=… …>…</HIVE>` source (head shown raw, then body).
    pub text: String,
    pub is_hive_envelope: bool,
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

/// First line of a tool result, plus its error flag.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub first_line: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    pub kind: GroupKind,
    /// Raw tool name (`Read`, `Grep`, …).
    pub name: String,
    /// Most relevant input field (path, pattern, url, query).
    pub hint: String,
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
    pub result: Option<ToolOutcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolBlock {
    pub name: String,
    pub hint: String,
    pub result: Option<ToolOutcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingBlock {
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
        block: ToolGroupBlock,
        ids: Vec<String>,
    },
    Run {
        block: RunBlock,
        id: String,
    },
    Tool {
        block: ToolBlock,
        id: String,
    },
}

impl Pending {
    fn into_block(self) -> DisplayBlock {
        match self {
            Pending::Group { block, .. } => DisplayBlock::ToolGroup(block),
            Pending::Run { block, .. } => DisplayBlock::Run(block),
            Pending::Tool { block, .. } => DisplayBlock::Tool(block),
        }
    }

    fn snapshot(&self) -> DisplayBlock {
        match self {
            Pending::Group { block, .. } => DisplayBlock::ToolGroup(block.clone()),
            Pending::Run { block, .. } => DisplayBlock::Run(block.clone()),
            Pending::Tool { block, .. } => DisplayBlock::Tool(block.clone()),
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
/// `push` returns the blocks *finalized* by that line, in display order;
/// `pending_blocks` snapshots what is still open (an aggregating tool group,
/// a running Bash) for live rendering; `flush` force-finalizes the rest.
pub struct TranscriptParser {
    pending: Vec<Pending>,
    prev_row_ms: Option<i64>,
    turn_start_ms: Option<i64>,
    last_assistant_text_ms: Option<i64>,
    turn_has_assistant_text: bool,
    tokens: i64,
    busy: bool,
    model: Option<String>,
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
            prev_row_ms: None,
            turn_start_ms: None,
            last_assistant_text_ms: None,
            turn_has_assistant_text: false,
            tokens: 0,
            busy: false,
            model: None,
        }
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

    /// Snapshot of the not-yet-finalized tail, in display order.
    pub fn pending_blocks(&self) -> Vec<DisplayBlock> {
        self.pending.iter().map(Pending::snapshot).collect()
    }

    /// Force-finalize everything pending (EOF, idle flush).
    pub fn flush(&mut self) -> Vec<DisplayBlock> {
        let mut out = Vec::new();
        self.drain_all(&mut out);
        out
    }

    fn drain_all(&mut self, out: &mut Vec<DisplayBlock>) {
        for item in self.pending.drain(..) {
            out.push(item.into_block());
        }
    }

    /// Emit the settled prefix of `pending`: complete runs/tools, and groups
    /// that stopped aggregating because a non-member was queued after them.
    /// Stops at the first incomplete item so display order is preserved.
    fn drain_settled(&mut self, out: &mut Vec<DisplayBlock>) {
        loop {
            let deliverable = match self.pending.first() {
                Some(Pending::Group { .. }) => self.pending.len() > 1,
                Some(item) => item.is_complete(),
                None => false,
            };
            if !deliverable {
                break;
            }
            out.push(self.pending.remove(0).into_block());
        }
    }

    fn attach_result(&mut self, id: &str, outcome: ToolOutcome) {
        if id.is_empty() {
            return;
        }
        for item in &mut self.pending {
            match item {
                Pending::Group { block, ids } => {
                    if let Some(i) = ids.iter().position(|x| x == id) {
                        block.members[i].result = Some(outcome);
                        return;
                    }
                }
                Pending::Run { block, id: rid } => {
                    if rid == id {
                        block.result = Some(outcome);
                        return;
                    }
                }
                Pending::Tool { block, id: rid } => {
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
                            out.push(DisplayBlock::WorkedFor(WorkedForBlock { duration_secs }));
                        }
                        self.turn_start_ms = ts.as_ref().map(|t| t.epoch_ms);
                        self.last_assistant_text_ms = None;
                        self.turn_has_assistant_text = false;
                        out.push(DisplayBlock::User(UserBlock {
                            text: body.to_string(),
                            is_hive_envelope: hive_envelope(body).is_some(),
                            timestamp: ts.clone(),
                        }));
                        self.busy = true;
                    } else {
                        out.push(DisplayBlock::Assistant(AssistantBlock {
                            markdown: body.to_string(),
                            timestamp: ts.clone(),
                        }));
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
                    out.push(DisplayBlock::Thinking(ThinkingBlock { duration_secs }));
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
                            result: None,
                        };
                        match self.pending.last_mut() {
                            Some(Pending::Group { block, ids }) => {
                                block.members.push(member);
                                ids.push(id);
                            }
                            _ => self.pending.push(Pending::Group {
                                block: ToolGroupBlock {
                                    members: vec![member],
                                },
                                ids: vec![id],
                            }),
                        }
                    } else if name == "Bash" {
                        self.pending.push(Pending::Run {
                            block: RunBlock {
                                description: run_description(input),
                                result: None,
                            },
                            id,
                        });
                    } else {
                        self.pending.push(Pending::Tool {
                            block: ToolBlock {
                                name: name.to_string(),
                                hint: generic_hint(input),
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
                    let first_line = _clip(&text, 160).lines().next().unwrap_or("").to_string();
                    let outcome = ToolOutcome {
                        first_line,
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    };
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
    let first = res.first_line.trim();
    if first.is_empty() {
        return None;
    }
    Some(format!("\n  {DIM}⎿  {first}{RESET}"))
}

fn _user_line(text: &str) -> String {
    if let Some((sender, body)) = hive_envelope(text) {
        let body = _clip(body, 160);
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
        assert_eq!(g.members[0].result.as_ref().unwrap().first_line, "1\tfn a");
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
        assert_eq!(r.result.as_ref().unwrap().first_line, "Compiling hive");
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
                assert!(!u.is_hive_envelope);
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
}
