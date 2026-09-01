// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::model::{
    image_chip, parse_hive_message, parse_timestamp, summarize_images, AssistantBlock,
    DisplayBlock, Entry, GroupKind, GroupMember, RunBlock, ThinkingBlock, Timestamp, ToolBlock,
    ToolGroupBlock, ToolOutcome, UserBlock, WorkedForBlock, AGENT_ICONS,
};

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
    ultra: bool,
    image_count: usize,
    agent_icons: HashMap<String, char>,
    queued_texts: HashSet<String>,
}

impl Default for TranscriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptParser {
    /// Pick this sender's avatar: hash its name into [`AGENT_ICONS`], then
    /// walk forward past anything a teammate already took, so a team's
    /// members never collide until the pool is exhausted. Stable — the same
    /// transcript always deals the same icons.
    fn agent_icon(&mut self, sender: &str) -> char {
        if let Some(&ch) = self.agent_icons.get(sender) {
            return ch;
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in sender.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let start = (hash % AGENT_ICONS.len() as u64) as usize;
        let taken: Vec<char> = self.agent_icons.values().copied().collect();
        let mut chosen = AGENT_ICONS[start];
        for step in 0..AGENT_ICONS.len() {
            let cand = AGENT_ICONS[(start + step) % AGENT_ICONS.len()];
            if !taken.contains(&cand) {
                chosen = cand;
                break;
            }
        }
        self.agent_icons.insert(sender.to_string(), chosen);
        chosen
    }

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
            ultra: false,
            image_count: 0,
            agent_icons: HashMap::new(),
            queued_texts: HashSet::new(),
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
    /// Epoch ms of the running turn's opening message, for a live timer.
    pub fn turn_started_ms(&self) -> Option<i64> {
        self.turn_start_ms
    }

    /// Output tokens counted so far.
    pub fn tokens(&self) -> i64 {
        self.tokens
    }

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

    /// Pick up session-level state from a line the viewer is about to skip.
    /// The tail window holds the last few hundred events; `ultra_effort_enter`
    /// is announced once and never repeated, so it usually sits above that
    /// line and would otherwise be lost.
    pub fn note_session_state(&mut self, raw: &str) {
        if !raw.contains("ultra_effort_enter") {
            return;
        }
        let Ok(row) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        if row.get("type").and_then(Value::as_str) == Some("attachment")
            && row
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str)
                == Some("ultra_effort_enter")
        {
            self.ultra = true;
        }
    }

    /// Reasoning-effort level from the most recent row carrying one — or
    /// `ultra`, which the assistant rows never say: it arrives once as an
    /// `ultra_effort_enter` attachment and has no recorded counterpart for
    /// leaving, so it holds for the rest of the transcript.
    pub fn effort(&self) -> Option<&str> {
        if self.ultra {
            return Some("ultra");
        }
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

    /// A message that arrived while the turn was already running: claude
    /// queues it, folds it into the turn, and records it *only* as a
    /// `queued_command` attachment — no `user` row ever follows. Peer HIVE
    /// envelopes and the human's own mid-turn messages both land here, so a
    /// viewer that reads only `user` rows silently drops them.
    ///
    /// System plumbing (task notifications, which carry no origin) stays out
    /// of the transcript.
    fn push_queued_command(&mut self, row: &Value, out: &mut Vec<Entry>) {
        let att = match row.get("attachment") {
            Some(a) if a.get("type").and_then(Value::as_str) == Some("queued_command") => a,
            _ => return,
        };
        let Some(prompt) = att.get("prompt").and_then(Value::as_str) else {
            return;
        };
        let from_human = att
            .get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(Value::as_str)
            == Some("human");
        let hive = parse_hive_message(prompt).map(|mut m| {
            m.icon = m.from.clone().map(|sender| self.agent_icon(&sender));
            m
        });
        if hive.is_none() && !from_human {
            return;
        }
        // The same text sometimes also lands as a `user` row; show it once.
        self.queued_texts.insert(prompt.trim().to_string());
        let ts = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        self.drain_all(out);
        out.push(Entry {
            id: self.alloc_id(),
            block: DisplayBlock::User(UserBlock {
                text: prompt.to_string(),
                hive,
                timestamp: ts,
            }),
        });
    }

    /// The queue's terminal states, from an append-only log: `dequeue` — it
    /// left the queue to open its own turn, and a `user` row follows — or
    /// `remove` with `absorbed_mid_turn`, folded into the turn already
    /// running, where no `user` row ever comes. Both mean the model saw it.
    ///
    /// Absorption usually also writes a `queued_command` attachment, which
    /// is the richer record (it carries the origin) and is handled above.
    /// A few absorptions leave only these rows, so a HIVE envelope that
    /// reaches its terminal state unrendered is drawn from here.
    fn push_absorbed_queue_row(&mut self, row: &Value, out: &mut Vec<Entry>) {
        if row.get("operation").and_then(Value::as_str) != Some("remove")
            || row.get("reason").and_then(Value::as_str) != Some("absorbed_mid_turn")
        {
            return;
        }
        let Some(content) = row.get("content").and_then(Value::as_str) else {
            return;
        };
        if self.queued_texts.contains(content.trim()) {
            return;
        }
        let Some(mut hive) = parse_hive_message(content) else {
            return;
        };
        hive.icon = hive.from.clone().map(|sender| self.agent_icon(&sender));
        self.queued_texts.insert(content.trim().to_string());
        let ts = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        self.drain_all(out);
        out.push(Entry {
            id: self.alloc_id(),
            block: DisplayBlock::User(UserBlock {
                text: content.to_string(),
                hive: Some(hive),
                timestamp: ts,
            }),
        });
    }

    /// Close the assistant's turn with its `Worked for` marker and start the
    /// human's. Idempotent within a row: the second content block of the same
    /// message finds the turn already open.
    fn open_user_turn(&mut self, ts: &Option<Timestamp>, out: &mut Vec<Entry>) {
        if self.turn_has_assistant_text {
            let duration_secs = match (self.turn_start_ms, self.last_assistant_text_ms) {
                (Some(start), Some(end)) if end >= start => Some((end - start) as f64 / 1000.0),
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
        if kind == "attachment" {
            if row
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str)
                == Some("ultra_effort_enter")
            {
                self.ultra = true;
            }
            self.push_queued_command(&row, &mut out);
            return out;
        }
        if kind == "queue-operation" {
            self.push_absorbed_queue_row(&row, &mut out);
            return out;
        }
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
        // A user message is assembled whole — text and pictures in the order
        // they were written — and emitted as one band once the row is read.
        let mut parts: Vec<String> = Vec::new();
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
                    if is_user {
                        parts.push(body.to_string());
                    } else {
                        self.drain_all(&mut out);
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
                "image" if is_user => {
                    self.image_count += 1;
                    parts.push(image_chip(self.image_count));
                }
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let body = block.get("content").unwrap_or(&null);
                    let text = match body.as_str() {
                        Some(s) => s.to_string(),
                        // An array body is serialized whole — which used to
                        // pour a screenshot's base64 into the outcome text,
                        // thousands of wrapped lines of it. Images are
                        // described instead.
                        None => {
                            serde_json::to_string(&summarize_images(body, &mut self.image_count))
                                .unwrap_or_default()
                        }
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
        if !parts.is_empty() {
            self.drain_all(&mut out);
            self.open_user_turn(&ts, &mut out);
            let text = parts.join("\n");
            // A queued message already rendered from its attachment row; do
            // not draw it twice.
            if !self.queued_texts.remove(&text) {
                let hive = parse_hive_message(&text).map(|mut m| {
                    m.icon = m.from.clone().map(|sender| self.agent_icon(&sender));
                    m
                });
                out.push(Entry {
                    id: self.alloc_id(),
                    block: DisplayBlock::User(UserBlock {
                        text,
                        hive,
                        timestamp: ts.clone(),
                    }),
                });
            }
            self.busy = true;
        }
        if let Some(t) = ts.as_ref() {
            self.prev_row_ms = Some(t.epoch_ms);
        }
        out
    }
}
