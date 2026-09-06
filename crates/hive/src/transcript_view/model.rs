use serde_json::Value;

/// Storage cap for one tool result's full text; longer results are cut at a
/// char boundary and flagged `truncated`.
pub const TOOL_RESULT_MAX_BYTES: usize = 512 * 1024;

pub(crate) fn clip(text: &str, limit: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(limit) {
        None => text.to_string(),
        Some((cut, _)) => format!("{} …", &text[..cut]),
    }
}

/// A HIVE envelope as it reaches a claude transcript, with the tag parsed
/// into fields instead of shown raw.
///
/// Five carriers exist. Four land in a `user` row: bare (typed straight
/// into the pane), claude's session-inbox injection at turn start or folded
/// in mid-turn (a lead line plus a trailing safety paragraph), and the
/// retired `<channel …>` wrapper still sitting in old transcripts. The fifth
/// — a message absorbed while the turn was already running — leaves no
/// `user` row at all and is read off its `queued_command` attachment or
/// `queue-operation` row instead (the parser's `push_queued_command` and
/// `push_absorbed_queue_row`).
#[derive(Debug, Clone, PartialEq)]
pub struct HiveMessage {
    pub from: Option<String>,
    pub artifact: Option<String>,
    pub body: String,
    /// The envelope arrived inside claude's peer-message wrapper rather than
    /// on its own.
    pub injected: bool,
    /// The wrapper said the message folded into a turn already in flight.
    pub mid_turn: bool,
    /// Which agent avatar this sender drew (see [`AGENT_ICONS`]). Assigned by
    /// the parser, which is the only layer that sees every sender in a
    /// transcript and can keep them distinct.
    pub icon: Option<char>,
}

/// Agent avatars for HIVE senders (Nerd Font: fa-robot, oct-hubot,
/// md-robot_happy and its outline, fa-android, fa-reddit_alien,
/// md-space_invaders). Every sender in a transcript draws a different one
/// until the pool runs out.
pub const AGENT_ICONS: [char; 7] = [
    '\u{ee0d}',
    '\u{f477}',
    '\u{f1719}',
    '\u{f171a}',
    '\u{f17b}',
    '\u{f281}',
    '\u{f0bc9}',
];

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

/// Peel the `<cross-session-message from="…">` tag hive's inbox sender puts
/// around an envelope so the receiver's own message card draws it clean
/// (`claude_sessions::peer_card_envelope`). Anything else passes through.
fn strip_peer_card_tag(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("<cross-session-message") else {
        return text;
    };
    let Some(gt) = rest.find('>') else {
        return text;
    };
    let inner = rest[gt + 1..].trim();
    inner
        .strip_suffix("</cross-session-message>")
        .unwrap_or(inner)
        .trim()
}

/// Peel claude's peer-message wrapper: the lead line above the envelope and
/// the safety paragraph below it, then the peer-card tag inside them.
/// Returns the core plus (injected, mid_turn).
fn strip_injection_wrapper(text: &str) -> (&str, bool, bool) {
    let trimmed = strip_channel_wrapper(text.trim());
    for (lead, mid) in [(INJECT_LEAD_MID, true), (INJECT_LEAD, false)] {
        if let Some(rest) = trimmed.strip_prefix(lead) {
            let rest = rest.trim_start();
            let core = match rest.find(INJECT_TAIL) {
                Some(i) => &rest[..i],
                None => rest,
            };
            return (strip_peer_card_tag(core.trim()), true, mid);
        }
    }
    (strip_peer_card_tag(trimmed), false, false)
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
        artifact: None,
        body: core[gt + 1..body_end].trim().to_string(),
        injected,
        mid_turn,
        icon: None,
    };
    for attr in tag.split_whitespace() {
        let Some((key, value)) = attr.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        // Unknown attrs (the retired `msgId`/`reply-to` in old transcripts
        // among them) are skipped, not errors.
        let slot = match key {
            "from" => &mut msg.from,
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

pub(super) fn parse_timestamp(s: &str) -> Option<Timestamp> {
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
    pub(super) fn new(text: String, is_error: bool) -> Self {
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
    /// line only.
    pub fn first_line(&self) -> String {
        clip(&self.text, 160)
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

/// grok's inline picture chip (xai-grok-pager-render prompt_images.rs
/// `display_text`): path-free, numbered, and nothing else — `[Image #1]`.
/// The payload is never read; a pasted screenshot is ~600KB of base64 and
/// the parser holds every entry for the session.
pub(super) fn image_chip(index: usize) -> String {
    format!("[Image #{index}]")
}

/// Replace every `image` block inside a tool result with its chip, so the
/// payload never reaches the outcome text.
pub(super) fn summarize_images(body: &Value, next_index: &mut usize) -> Value {
    let Some(items) = body.as_array() else {
        return body.clone();
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("image") {
                    *next_index += 1;
                    Value::String(image_chip(*next_index))
                } else {
                    item.clone()
                }
            })
            .collect(),
    )
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
    /// Raw markdown source; rendered by the `grok_md` engine.
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
