//! The cvim toolkit: embedded popup-editor assets plus the hidden helper
//! subcommands the `cvim-command` bash asset calls back into.
//!
//! The helpers (`cvim-sendback`, `cvim-payload`, `cvim-list`, `cvim-seed`,
//! `cvim-session`, `cvim-profile`) are `hive cvim-*` hidden subcommands the
//! driver invokes through `$hive_bin`; the bash driver and its vim/protocol
//! resources are embedded and materialized under
//! `$HIVE_HOME/core_assets/cvim/` at first use.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::adapters::base::{DateTime, Message, SessionAdapter};

// ---------------------------------------------------------------------------
// embedded assets + materialization
// ---------------------------------------------------------------------------

const CVIM_COMMAND: &str = include_str!("../assets/cvim/bin/cvim-command");
const MENU_VIM: &str = include_str!("../assets/cvim/resources/menu.vim");
const PROTOCOL_JSON: &str = include_str!("../assets/cvim/resources/cvim_edit_protocol.json");

/// Write the embedded cvim asset tree under `$HIVE_HOME/core_assets/cvim/`
/// (rewriting any file whose on-disk copy drifted from the embedded content)
/// and return the `cvim-command` path.
pub fn materialize_assets() -> anyhow::Result<PathBuf> {
    let root = crate::paths::hive_home().join("core_assets").join("cvim");
    crate::assets::materialize_asset_tree(
        &root,
        &[
            ("bin/cvim-command", CVIM_COMMAND, true),
            ("resources/menu.vim", MENU_VIM, false),
            ("resources/cvim_edit_protocol.json", PROTOCOL_JSON, false),
        ],
    )?;
    Ok(root.join("bin").join("cvim-command"))
}

// ---------------------------------------------------------------------------
// transcript helpers shared by the cvim-* subcommands
// ---------------------------------------------------------------------------

/// Probe order for the adapter that claims a transcript.
const ADAPTER_NAMES: [&str; 3] = ["claude", "codex", "grok"];

fn adapter_for(name: &str) -> Option<Box<dyn SessionAdapter>> {
    match name {
        "claude" => Some(Box::new(crate::adapters::claude::ClaudeAdapter)),
        "codex" => Some(Box::new(crate::adapters::codex::CodexAdapter)),
        "grok" => Some(Box::new(crate::adapters::grok::GrokAdapter)),
        _ => None,
    }
}

fn detect_adapter_for_transcript(path: &Path) -> Option<Box<dyn SessionAdapter>> {
    for name in ADAPTER_NAMES {
        let adapter = adapter_for(name)?;
        if adapter.read_meta(path).is_some() {
            return Some(adapter);
        }
    }
    None
}

fn resolve_hive_runtime_session_id(pane_id: &str) -> (bool, Option<String>) {
    let workspace = crate::tmux::display_value(pane_id, "#{@hive-workspace}").unwrap_or_default();
    if workspace.is_empty() {
        return (false, None);
    }
    let Some(payload) = crate::hived::request_runtime_snapshot(&workspace, pane_id) else {
        return (false, None);
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        return (false, None);
    }
    let Some(Value::Object(snapshot)) = payload.get("snapshot") else {
        // No hived snapshot for this pane: not hived-managed truth. The
        // adapter is the authority, so fall through to it.
        return (false, None);
    };
    if snapshot.get("_sessionIdFresh") == Some(&Value::Bool(false)) {
        return (true, None);
    }
    if let Some(Value::String(session_id)) = snapshot.get("sessionId") {
        if !session_id.is_empty() && session_id != "unresolved" {
            return (true, Some(session_id.clone()));
        }
    }
    (false, None)
}

#[derive(Debug)]
pub(crate) struct RecentEntry {
    pub offset: usize,
    pub timestamp: String,
    pub preview: String,
    pub text: String,
}

/// Up to `limit` most-recent assistant messages, newest first; `limit == 0`
/// lists nothing. (`cvim-list` defaults to 10 and the bash driver never
/// passes a limit.)
pub(crate) fn list_recent_assistant_messages(file_path: &Path, limit: usize) -> Vec<RecentEntry> {
    let adapter = detect_adapter_for_transcript(file_path);
    with_assistant_texts_newest_first(file_path, adapter.as_deref(), |hits| {
        hits.take(limit)
            .enumerate()
            .map(|(offset, (text, timestamp))| RecentEntry {
                offset,
                timestamp,
                preview: build_preview(&text),
                text,
            })
            .collect()
    })
}

fn read_lossy(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Walk the non-empty assistant texts newest first, each paired with its
/// `HH:MM` timestamp, and hand `take` the iterator. When an adapter claims
/// the transcript its `iter_messages` is collected into a Vec and walked in
/// reverse, so the whole file is parsed before `take` sees the first hit;
/// only the raw claude JSONL fallback (the file is still read whole) parses
/// its lines lazily from the tail, stopping where `take` stops.
fn with_assistant_texts_newest_first<R>(
    file_path: &Path,
    adapter: Option<&dyn SessionAdapter>,
    take: impl FnOnce(&mut dyn Iterator<Item = (String, String)>) -> R,
) -> R {
    match adapter {
        Some(adapter) => {
            let messages: Vec<Message> = adapter.iter_messages(file_path).collect();
            let mut hits = messages
                .iter()
                .rev()
                .filter(|message| message.role == "assistant")
                .filter_map(|message| {
                    let text = assistant_text_from_normalized_message(message);
                    (!text.is_empty())
                        .then(|| (text, format_timestamp_dt(message.timestamp.as_ref())))
                });
            take(&mut hits)
        }
        None => {
            let content = read_lossy(file_path).unwrap_or_default();
            let mut hits = content.lines().rev().filter_map(|line| {
                let obj = crate::adapters::base::safe_json_loads(line)?;
                if obj.get("type").and_then(Value::as_str) != Some("message") {
                    return None;
                }
                let message = obj.get("message")?.as_object()?;
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    return None;
                }
                let text = assistant_text_from_raw_claude_message(message);
                (!text.is_empty()).then(|| (text, format_timestamp_value(obj.get("timestamp"))))
            });
            take(&mut hits)
        }
    }
}

/// `HH:MM` of `epoch_secs` in the local timezone.
fn local_hhmm(epoch_secs: f64) -> String {
    let t = epoch_secs.floor() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn format_timestamp_dt(value: Option<&DateTime>) -> String {
    match value {
        None => String::new(),
        // A timestamp without a UTC offset is taken as already local: its
        // own wall-clock fields survive.
        Some(dt) if dt.utc_offset_secs.is_none() => format!("{:02}:{:02}", dt.hour, dt.minute),
        Some(dt) => local_hhmm(dt.timestamp()),
    }
}

fn format_timestamp_value(value: Option<&Value>) -> String {
    match crate::adapters::base::parse_iso_timestamp(value) {
        Some(dt) => format_timestamp_dt(Some(&dt)),
        None => String::new(),
    }
}

fn build_preview(text: &str) -> String {
    const WIDTH: usize = 80;
    for line in text.split('\n') {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        return if stripped.chars().count() <= WIDTH {
            stripped.to_string()
        } else {
            let mut cut: String = stripped.chars().take(WIDTH - 1).collect();
            cut.push('…');
            cut
        };
    }
    String::new()
}

fn plan_part(tool_input: &Map<String, Value>) -> Option<String> {
    let plan = tool_input.get("plan").and_then(Value::as_str)?;
    if plan.trim().is_empty() {
        return None;
    }
    let mut header = String::new();
    if let Some(title) = tool_input.get("title").and_then(Value::as_str) {
        if !title.trim().is_empty() {
            header = format!("Propose Specification title: \"{}\"\n\n", title.trim());
        }
    }
    Some(format!(
        "{header}Specification for approval:\n\n{}",
        plan.trim()
    ))
}

fn assistant_text_from_raw_claude_message(message: &Map<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let content = match message.get("content") {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    };
    for item in content {
        let Value::Object(item) = item else { continue };
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.trim().is_empty() {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
            Some("tool_use")
                if matches!(
                    item.get("name").and_then(Value::as_str),
                    Some("ExitSpecMode") | Some("ExitPlanMode")
                ) =>
            {
                if let Some(Value::Object(tool_input)) = item.get("input") {
                    if let Some(part) = plan_part(tool_input) {
                        parts.push(part);
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n").trim().to_string()
}

fn assistant_text_from_normalized_message(message: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in &message.parts {
        match item.kind.as_str() {
            "text" => {
                let text = item.text.as_deref().unwrap_or("");
                if !text.trim().is_empty() {
                    parts.push(text.trim_end_matches('\n').to_string());
                }
            }
            "tool_use"
                if matches!(
                    item.tool_name.as_deref(),
                    Some("ExitSpecMode") | Some("ExitPlanMode")
                ) =>
            {
                if let Some(tool_input) = &item.tool_input {
                    if let Some(part) = plan_part(tool_input) {
                        parts.push(part);
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n").trim().to_string()
}

/// The Nth assistant message from the end (0=last, 1=second-to-last, ...).
pub(crate) fn extract_last_assistant_text(file_path: &Path, offset: usize) -> String {
    let adapter = detect_adapter_for_transcript(file_path);
    let offset = match adapter.as_deref() {
        Some(adapter) => resolve_assistant_offset(file_path, offset, Some(adapter)),
        None => offset,
    };
    with_assistant_texts_newest_first(file_path, adapter.as_deref(), |hits| {
        hits.nth(offset).map(|(text, _)| text).unwrap_or_default()
    })
}

pub(crate) fn resolve_assistant_offset(
    file_path: &Path,
    offset: usize,
    adapter: Option<&dyn SessionAdapter>,
) -> usize {
    let owned;
    let adapter = match adapter {
        Some(adapter) => adapter,
        None => match detect_adapter_for_transcript(file_path) {
            Some(found) => {
                owned = found;
                &*owned
            }
            None => return offset,
        },
    };
    if adapter.name() != "codex" {
        return offset;
    }
    let messages: Vec<Message> = adapter.iter_messages(file_path).collect();
    resolve_codex_skill_turn_offset(&messages, offset)
}

fn resolve_codex_skill_turn_offset(messages: &[Message], offset: usize) -> usize {
    let tail_turn_id = messages.iter().rev().find_map(message_turn_id);
    let Some(tail_turn_id) = tail_turn_id else {
        return offset;
    };
    let tail_turn: Vec<&Message> = messages
        .iter()
        .filter(|m| message_turn_id(m).as_deref() == Some(&tail_turn_id))
        .collect();
    if !turn_invokes_codex_command_skill(&tail_turn) {
        return offset;
    }
    let synthetic = tail_turn
        .iter()
        .filter(|m| is_codex_commentary_assistant_message(m))
        .count();
    offset + synthetic
}

fn message_turn_id(message: &Message) -> Option<String> {
    match message.raw.get("turn_id") {
        Some(Value::String(turn_id)) if !turn_id.is_empty() => Some(turn_id.clone()),
        _ => None,
    }
}

/// `^\s*\$(?:cvim|vim)(?:\s|$)`
fn matches_codex_command_skill(text: &str) -> bool {
    let trimmed = text.trim_start();
    for prefix in ["$cvim", "$vim"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return true;
            }
        }
    }
    false
}

fn turn_invokes_codex_command_skill(messages: &[&Message]) -> bool {
    for message in messages {
        if message.role != "user" {
            continue;
        }
        for item in &message.parts {
            if item.kind != "text" {
                continue;
            }
            if let Some(text) = &item.text {
                if matches_codex_command_skill(text) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_codex_commentary_assistant_message(message: &Message) -> bool {
    if message.role != "assistant" {
        return false;
    }
    let Some(Value::Object(payload)) = message.raw.get("payload") else {
        return false;
    };
    payload.get("type").and_then(Value::as_str) == Some("message")
        && payload.get("phase").and_then(Value::as_str) == Some("commentary")
}

fn write_seed(dst: &Path, preferred: Option<&Path>, offset: usize) -> std::io::Result<()> {
    match preferred {
        Some(preferred) => {
            let text = extract_last_assistant_text(preferred, offset);
            if text.is_empty() {
                fs::write(dst, "")
            } else {
                fs::write(dst, format!("{text}\n"))
            }
        }
        None => fs::write(dst, ""),
    }
}

fn resolve_transcript_path_for_pane(pane_id: &str, cwd: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }
    let profile = crate::agent_cli::detect_profile_for_pane(pane_id)?;
    let adapter = adapter_for(profile.name)?;
    let (hive_managed, mut session_id) = resolve_hive_runtime_session_id(pane_id);
    if !hive_managed {
        session_id = adapter.resolve_current_session_id(pane_id);
    }
    let session_id = session_id?;
    let transcript_path = adapter.find_session_file(&session_id, Some(cwd))?;
    if transcript_path.is_file() {
        return Some(transcript_path.to_string_lossy().into_owned());
    }
    None
}

// ---------------------------------------------------------------------------
// cvim-sendback
// ---------------------------------------------------------------------------

const OK: i32 = 0;
const REFUSED: i32 = 1;
const NO_NATIVE_ADDRESS: i32 = 10;

type Fields = Vec<(&'static str, String)>;

fn claude_sendback(pane: &str, text: Option<&str>, interrupt: bool) -> (i32, Fields) {
    use crate::adapters::{claude_bg, claude_view};

    let Some(job_id) = claude_bg::job_id_for_pane(pane) else {
        if claude_view::interactive_claude_pid(pane).is_some() {
            // A plain interactive claude TUI: no job to address, but its
            // composer is its own, so keystrokes are safe.
            return (
                NO_NATIVE_ADDRESS,
                vec![
                    ("route", "tmuxKeys".into()),
                    ("why", "no_job_record".into()),
                ],
            );
        }
        // An attach viewer (its composer belongs to whatever session it
        // shows) or nothing at all: typing here would land in a stranger's
        // turn, or in the pane shell.
        return (
            REFUSED,
            vec![
                ("route", "none".into()),
                ("why", "no_job_record_and_no_interactive_claude".into()),
            ],
        );
    };

    let mut fields: Fields = vec![("route", "claudeJobPipe".into()), ("job", job_id.clone())];
    if interrupt {
        let result = claude_bg::interrupt_job(&job_id, "claude");
        fields.push((
            "interrupt",
            if result.ok {
                result.confirmed.clone()
            } else {
                "failed".into()
            },
        ));
        if !result.ok && text.is_none() {
            fields.push(("why", result.why));
            return (REFUSED, fields);
        }
    }
    let Some(text) = text else {
        return (OK, fields);
    };
    let result = claude_bg::type_into_job(&job_id, text, "claude");
    fields.push((
        "send",
        if result.ok {
            result.confirmed.clone()
        } else {
            "failed".into()
        },
    ));
    if !result.ok {
        fields.push(("why", result.why));
        return (REFUSED, fields);
    }
    (OK, fields)
}

fn codex_sendback(pane: &str, text: Option<&str>, interrupt: bool) -> (i32, Fields) {
    use crate::adapters::codex_app_server;
    daemon_sendback(
        pane,
        text,
        interrupt,
        ("codexThread", "no_recorded_thread"),
        codex_app_server::thread_id_for_pane(pane).is_some(),
        codex_app_server::interrupt_pane,
        codex_app_server::send_to_pane,
    )
}

fn grok_sendback(pane: &str, text: Option<&str>, interrupt: bool) -> (i32, Fields) {
    use crate::adapters::grok_leader;
    daemon_sendback(
        pane,
        text,
        interrupt,
        ("grokSession", "no_recorded_session"),
        grok_leader::session_id_for_pane(pane).is_some(),
        grok_leader::interrupt_pane,
        grok_leader::send_to_pane,
    )
}

/// The daemon-addressed CLIs share one sendback shape: a pane with no
/// recorded address falls back to the composer, otherwise the interrupt
/// and the send each go through the daemon and report their verdict.
fn daemon_sendback(
    pane: &str,
    text: Option<&str>,
    interrupt: bool,
    (route, no_address_why): (&str, &str),
    has_address: bool,
    interrupt_pane: fn(&str) -> Option<&'static str>,
    send_to_pane: fn(&str, &str) -> Option<&'static str>,
) -> (i32, Fields) {
    if !has_address {
        return (
            NO_NATIVE_ADDRESS,
            vec![("route", "tmuxKeys".into()), ("why", no_address_why.into())],
        );
    }
    let mut fields: Fields = vec![("route", route.into())];
    if interrupt {
        let accepted = interrupt_pane(pane);
        fields.push(("interrupt", accepted.unwrap_or("failed").into()));
        if accepted.is_none() && text.is_none() {
            return (REFUSED, fields);
        }
    }
    let Some(text) = text else {
        return (OK, fields);
    };
    let accepted = send_to_pane(pane, text);
    fields.push(("send", accepted.unwrap_or("failed").into()));
    (if accepted.is_some() { OK } else { REFUSED }, fields)
}

fn is_slash_command(text: &str) -> bool {
    let stripped = text.trim();
    stripped.starts_with('/') && !stripped.contains('\n')
}

/// Route one sendback; `text` is None when the popup changed nothing.
fn sendback(pane: &str, profile: &str, text: Option<&str>, interrupt: bool) -> (i32, Fields) {
    if matches!(profile, "codex" | "grok") {
        if let Some(text) = text {
            if is_slash_command(text) {
                // A slash command is TUI vocabulary, not a prompt: it goes to
                // the composer, the same route `hive compact` falls back to.
                return (
                    NO_NATIVE_ADDRESS,
                    vec![
                        ("route", "tmuxKeys".into()),
                        ("why", "slash_command".into()),
                    ],
                );
            }
        }
    }
    match profile {
        "claude" => claude_sendback(pane, text, interrupt),
        "codex" => codex_sendback(pane, text, interrupt),
        "grok" => grok_sendback(pane, text, interrupt),
        _ => (
            NO_NATIVE_ADDRESS,
            vec![
                ("route", "tmuxKeys".into()),
                (
                    "why",
                    format!(
                        "profile_{}",
                        if profile.is_empty() {
                            "unknown"
                        } else {
                            profile
                        }
                    ),
                ),
            ],
        ),
    }
}

pub fn sendback_main(args: &[String]) -> i32 {
    if args.len() < 5 {
        eprintln!(
            "usage: hive cvim-sendback <pane> <profile> <send_file> <content_changed> <interrupt>"
        );
        return 1;
    }
    let (pane, profile, send_file, content_changed, interrupt) =
        (&args[0], &args[1], &args[2], &args[3], &args[4]);
    let text: Option<String> = if content_changed == "1" && !send_file.is_empty() {
        match read_lossy(Path::new(send_file)) {
            // Same trailing-newline trim the keystroke path applies.
            Some(content) => Some(content.trim_end_matches('\n').to_string()),
            None => {
                eprintln!("cvim-sendback: cannot read {send_file}");
                return 1;
            }
        }
    } else {
        None
    };
    let (code, fields) = sendback(pane, profile, text.as_deref(), interrupt != "0");
    println!(
        "{}",
        fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    code
}

// ---------------------------------------------------------------------------
// cvim-payload
// ---------------------------------------------------------------------------

struct Protocol {
    tag: String,
    on_attr: String,
    default_target: String,
    offset_target_format: String,
}

fn protocol() -> Protocol {
    let value: Value = serde_json::from_str(PROTOCOL_JSON).expect("embedded protocol json");
    let get = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .expect("protocol field")
            .to_string()
    };
    Protocol {
        tag: get("tag"),
        on_attr: get("onAttr"),
        default_target: get("defaultTarget"),
        offset_target_format: get("offsetTargetFormat"),
    }
}

fn resolve_target(protocol: &Protocol, offset: usize) -> String {
    if offset == 0 {
        return protocol.default_target.clone();
    }
    protocol
        .offset_target_format
        .replace("{n}", &(offset + 1).to_string())
}

fn build_hunks_only_diff(orig: &str, edited: &str) -> String {
    let raw = difflib::unified_diff(orig, edited);
    let lines: Vec<&str> = raw
        .iter()
        .map(String::as_str)
        .filter(|line| !line.starts_with("--- ") && !line.starts_with("+++ "))
        .collect();
    lines.concat().trim_end_matches('\n').to_string()
}

fn build_payload(orig: &str, edited: &str, mode: &str, offset: usize) -> String {
    if mode == "diff" {
        let protocol = protocol();
        let target = resolve_target(&protocol, offset);
        let diff = build_hunks_only_diff(orig, edited);
        return format!(
            "<{tag} {attr}=\"{target}\">\n{diff}\n</{tag}>",
            tag = protocol.tag,
            attr = protocol.on_attr,
        );
    }
    edited.trim_end_matches('\n').to_string()
}

pub fn payload_main(args: &[String]) -> i32 {
    if args.len() < 4 {
        eprintln!("usage: hive cvim-payload <orig> <edited> <dst> <mode>");
        return 1;
    }
    let (Some(orig), Some(edited)) = (
        read_lossy(Path::new(&args[0])),
        read_lossy(Path::new(&args[1])),
    ) else {
        eprintln!("cvim-payload: cannot read input files");
        return 1;
    };
    let dst = PathBuf::from(&args[2]);
    let mode = &args[3];
    let tmpdir = dst.parent().unwrap_or_else(|| Path::new("."));
    let mut offset: usize = fs::read_to_string(tmpdir.join("offset"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let transcript_path_file = tmpdir.join("transcript_path");
    if !tmpdir.join("menu_selected").is_file() && transcript_path_file.is_file() {
        if let Ok(transcript) = fs::read_to_string(&transcript_path_file) {
            offset = resolve_assistant_offset(Path::new(transcript.trim()), offset, None);
        }
    }
    let payload = build_payload(&orig, &edited, mode, offset);
    let payload = payload.strip_suffix('\n').unwrap_or(&payload);
    if fs::write(&dst, payload).is_err() {
        eprintln!("cvim-payload: cannot write {}", dst.display());
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// cvim-list
// ---------------------------------------------------------------------------

pub fn list_main(args: &[String]) -> i32 {
    if !(args.len() == 3 || args.len() == 4) {
        eprintln!("usage: cvim-list <transcript> <seeds_dir> <menu_json> [limit]");
        return 1;
    }
    let transcript = Path::new(&args[0]);
    let seeds_dir = Path::new(&args[1]);
    let menu_json = Path::new(&args[2]);
    let limit: usize = if args.len() == 4 {
        match args[3].parse() {
            Ok(limit) => limit,
            Err(_) => {
                eprintln!("cvim-list: invalid limit {}", args[3]);
                return 1;
            }
        }
    } else {
        10
    };

    if fs::create_dir_all(seeds_dir).is_err() {
        eprintln!("cvim-list: cannot create {}", seeds_dir.display());
        return 1;
    }
    let entries = list_recent_assistant_messages(transcript, limit);

    let mut menu: Vec<Value> = Vec::new();
    for entry in &entries {
        let seed_path = seeds_dir.join(format!("{}.md", entry.offset));
        let text = if !entry.text.is_empty() && !entry.text.ends_with('\n') {
            format!("{}\n", entry.text)
        } else {
            entry.text.clone()
        };
        if fs::write(&seed_path, text).is_err() {
            eprintln!("cvim-list: cannot write {}", seed_path.display());
            return 1;
        }
        let timestamp = if entry.timestamp.is_empty() {
            "--:--"
        } else {
            &entry.timestamp
        };
        let preview = if entry.preview.is_empty() {
            "(empty)"
        } else {
            &entry.preview
        };
        let mut row = Map::new();
        row.insert("offset".to_string(), Value::from(entry.offset));
        row.insert(
            "label".to_string(),
            Value::from(format!("{timestamp}  {preview}")),
        );
        menu.push(Value::Object(row));
    }
    let rendered = Value::Array(menu).to_string();
    if fs::write(menu_json, rendered).is_err() {
        eprintln!("cvim-list: cannot write {}", menu_json.display());
        return 1;
    }
    // The bash driver reads the entry count from stdout.
    println!("{}", entries.len());
    0
}

// ---------------------------------------------------------------------------
// cvim-seed / cvim-session / cvim-profile
// ---------------------------------------------------------------------------

pub fn seed_main(args: &[String]) -> i32 {
    if !(1..=3).contains(&args.len()) {
        eprintln!("usage: cvim-seed <dst> [preferred] [offset]");
        return 1;
    }
    let dst = Path::new(&args[0]);
    let preferred = match args.get(1) {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    };
    let offset: usize = match args.get(2) {
        Some(raw) if !raw.is_empty() => match raw.parse() {
            Ok(offset) => offset,
            Err(_) => {
                eprintln!("cvim-seed: invalid offset {raw}");
                return 1;
            }
        },
        _ => 0,
    };
    if write_seed(dst, preferred.as_deref(), offset).is_err() {
        eprintln!("cvim-seed: cannot write {}", dst.display());
        return 1;
    }
    0
}

pub fn session_main(args: &[String]) -> i32 {
    if !(args.len() == 1 || args.len() == 2) {
        eprintln!("usage: cvim-session <cwd> [pane_id]");
        return 1;
    }
    let pane_id = args.get(1).map(String::as_str).unwrap_or("");
    if let Some(transcript_path) = resolve_transcript_path_for_pane(pane_id, &args[0]) {
        println!("{transcript_path}");
    }
    0
}

/// Profile name for a pane, printed for the bash driver (nothing when no
/// agent CLI is recognized there).
pub fn profile_main(args: &[String]) -> i32 {
    let pane = args.first().map(String::as_str).unwrap_or("");
    if let Some(profile) = crate::agent_cli::detect_profile_for_pane(pane) {
        println!("{}", profile.name);
    }
    0
}

// ---------------------------------------------------------------------------
// difflib.unified_diff (default n=3, no junk, autojunk as CPython)
// ---------------------------------------------------------------------------

mod difflib {
    use std::collections::HashMap;

    type Opcode = (&'static str, usize, usize, usize, usize);

    fn split_keepends(s: &str) -> Vec<&str> {
        s.split_inclusive('\n').collect()
    }

    fn build_b2j<'a>(b: &[&'a str]) -> HashMap<&'a str, Vec<usize>> {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, line) in b.iter().enumerate() {
            b2j.entry(line).or_default().push(i);
        }
        // autojunk: elements appearing in more than 1% of a 200+-line b.
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            let popular: Vec<&str> = b2j
                .iter()
                .filter(|(_, idxs)| idxs.len() > ntest)
                .map(|(&elt, _)| elt)
                .collect();
            for elt in popular {
                b2j.remove(elt);
            }
        }
        b2j
    }

    fn find_longest_match(
        a: &[&str],
        b: &[&str],
        b2j: &HashMap<&str, Vec<usize>>,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, item) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = b2j.get(item) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = if j == 0 {
                        1
                    } else {
                        j2len.get(&(j - 1)).copied().unwrap_or(0) + 1
                    };
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    fn get_matching_blocks(a: &[&str], b: &[&str]) -> Vec<(usize, usize, usize)> {
        let b2j = build_b2j(b);
        let (la, lb) = (a.len(), b.len());
        let mut queue = vec![(0usize, la, 0usize, lb)];
        let mut matching: Vec<(usize, usize, usize)> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = find_longest_match(a, b, &b2j, alo, ahi, blo, bhi);
            if k > 0 {
                matching.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        matching.sort_unstable();
        let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
        let mut non_adjacent: Vec<(usize, usize, usize)> = Vec::new();
        for (i2, j2, k2) in matching {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    non_adjacent.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            non_adjacent.push((i1, j1, k1));
        }
        non_adjacent.push((la, lb, 0));
        non_adjacent
    }

    fn get_opcodes(a: &[&str], b: &[&str]) -> Vec<Opcode> {
        let (mut i, mut j) = (0usize, 0usize);
        let mut answer: Vec<Opcode> = Vec::new();
        for (ai, bj, size) in get_matching_blocks(a, b) {
            let tag: &'static str = if i < ai && j < bj {
                "replace"
            } else if i < ai {
                "delete"
            } else if j < bj {
                "insert"
            } else {
                ""
            };
            if !tag.is_empty() {
                answer.push((tag, i, ai, j, bj));
            }
            (i, j) = (ai + size, bj + size);
            if size > 0 {
                answer.push(("equal", ai, i, bj, j));
            }
        }
        answer
    }

    fn get_grouped_opcodes(a: &[&str], b: &[&str], n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = get_opcodes(a, b);
        if codes.is_empty() {
            codes.push(("equal", 0, 1, 0, 1));
        }
        if codes[0].0 == "equal" {
            let (tag, i1, i2, j1, j2) = codes[0];
            codes[0] = (
                tag,
                i1.max(i2.saturating_sub(n)),
                i2,
                j1.max(j2.saturating_sub(n)),
                j2,
            );
        }
        let last = codes.len() - 1;
        if codes[last].0 == "equal" {
            let (tag, i1, i2, j1, j2) = codes[last];
            codes[last] = (tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n));
        }
        let nn = n + n;
        let mut groups: Vec<Vec<Opcode>> = Vec::new();
        let mut group: Vec<Opcode> = Vec::new();
        for (tag, mut i1, i2, mut j1, j2) in codes {
            if tag == "equal" && i2 - i1 > nn {
                group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
                groups.push(std::mem::take(&mut group));
                i1 = i1.max(i2.saturating_sub(n));
                j1 = j1.max(j2.saturating_sub(n));
            }
            group.push((tag, i1, i2, j1, j2));
        }
        if !group.is_empty() && (group.len() != 1 || group[0].0 != "equal") {
            groups.push(group);
        }
        groups
    }

    fn format_range_unified(start: usize, stop: usize) -> String {
        let mut beginning = start + 1;
        let length = stop - start;
        if length == 1 {
            return beginning.to_string();
        }
        if length == 0 {
            beginning -= 1;
        }
        format!("{beginning},{length}")
    }

    /// `difflib.unified_diff(a, b, fromfile="", tofile="")`, one output line
    /// per element (lines keep their own line endings).
    pub fn unified_diff(a_text: &str, b_text: &str) -> Vec<String> {
        let a = split_keepends(a_text);
        let b = split_keepends(b_text);
        let mut out: Vec<String> = Vec::new();
        let mut started = false;
        for group in get_grouped_opcodes(&a, &b, 3) {
            if !started {
                started = true;
                out.push("--- \n".to_string());
                out.push("+++ \n".to_string());
            }
            let (first, last) = (group[0], group[group.len() - 1]);
            let file1_range = format_range_unified(first.1, last.2);
            let file2_range = format_range_unified(first.3, last.4);
            out.push(format!("@@ -{file1_range} +{file2_range} @@\n"));
            for (tag, i1, i2, j1, j2) in group {
                if tag == "equal" {
                    for line in &a[i1..i2] {
                        out.push(format!(" {line}"));
                    }
                    continue;
                }
                if tag == "replace" || tag == "delete" {
                    for line in &a[i1..i2] {
                        out.push(format!("-{line}"));
                    }
                }
                if tag == "replace" || tag == "insert" {
                    for line in &b[j1..j2] {
                        out.push(format!("+{line}"));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_lines(path: &Path, rows: &[Value]) {
        let mut out = String::new();
        for row in rows {
            out.push_str(&row.to_string());
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    fn make_claude_session(dir: &Path, messages: &[Value]) -> PathBuf {
        let path = dir.join("session.jsonl");
        let rows: Vec<Value> = messages
            .iter()
            .map(|m| serde_json::json!({"type": "message", "message": m}))
            .collect();
        write_lines(&path, &rows);
        path
    }

    // -- transcript helpers --------------------------------------------------

    #[test]
    fn test_extract_includes_exit_spec_mode_plan_with_text() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[serde_json::json!({"role": "assistant", "content": [
                {"type": "text", "text": "Summary text here."},
                {"type": "tool_use", "name": "ExitSpecMode",
                 "input": {"plan": "## Detailed plan", "title": "My Spec"}},
            ]})],
        );
        let result = extract_last_assistant_text(&session, 0);
        assert!(result.contains("Summary text here."));
        assert!(result.contains("Propose Specification title: \"My Spec\""));
        assert!(result.contains("Specification for approval:"));
        assert!(result.contains("## Detailed plan"));
    }

    #[test]
    fn test_extract_plan_only_without_text() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "name": "ExitSpecMode", "input": {"plan": "## Plan only"}},
            ]})],
        );
        let result = extract_last_assistant_text(&session, 0);
        assert!(result.contains("Specification for approval:"));
        assert!(result.contains("## Plan only"));
    }

    #[test]
    fn test_list_recent_assistant_messages_orders_newest_first() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer A"}]}),
                serde_json::json!({"role": "user", "content": [{"type": "text", "text": "u"}]}),
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer B"}]}),
            ],
        );
        let entries = list_recent_assistant_messages(&session, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].offset, 0);
        assert_eq!(entries[0].text, "answer B");
        assert_eq!(entries[1].offset, 1);
        assert_eq!(entries[1].text, "answer A");
        assert_eq!(entries[0].preview, "answer B");
        assert_eq!(entries[0].timestamp, "");
    }

    #[test]
    fn test_list_recent_assistant_messages_honors_limit_and_zero_lists_nothing() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer A"}]}),
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer B"}]}),
            ],
        );
        let one = list_recent_assistant_messages(&session, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].offset, 0);
        assert_eq!(one[0].text, "answer B");
        assert!(list_recent_assistant_messages(&session, 0).is_empty());
    }

    // -- cvim-payload --------------------------------------------------------

    fn build_payload_via_main(
        tmp: &Path,
        orig: &str,
        edited: &str,
        mode: &str,
        offset: Option<usize>,
    ) -> String {
        let orig_file = tmp.join("orig.md");
        let edited_file = tmp.join("edited.md");
        let send_file = tmp.join("send.txt");
        fs::write(&orig_file, orig).unwrap();
        fs::write(&edited_file, edited).unwrap();
        if let Some(offset) = offset {
            fs::write(tmp.join("offset"), offset.to_string()).unwrap();
        }
        let args: Vec<String> = [
            orig_file.to_str().unwrap(),
            edited_file.to_str().unwrap(),
            send_file.to_str().unwrap(),
            mode,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(payload_main(&args), 0);
        fs::read_to_string(&send_file).unwrap()
    }

    #[test]
    fn test_diff_payload_uses_comment_wrapper_with_default_target() {
        let tmp = TempDir::new().unwrap();
        let payload = build_payload_via_main(
            tmp.path(),
            "旧内容\n",
            "旧内容\n\n貌似可以了哦\n",
            "diff",
            None,
        );
        assert!(payload.starts_with("<comment on=\"previous_reply\">"));
        assert!(payload.ends_with("</comment>"));
        assert!(!payload.contains("--- "));
        assert!(!payload.contains("+++ "));
        assert!(payload.contains("@@"));
    }

    #[test]
    fn test_text_payload_is_bare_pass_through() {
        let tmp = TempDir::new().unwrap();
        let payload = build_payload_via_main(tmp.path(), "", "整理后的正文\n", "text", None);
        assert_eq!(payload, "整理后的正文");
        assert!(!payload.contains("<comment"));
    }

    #[test]
    fn test_diff_payload_with_offset_targets_indexed_reply() {
        let tmp = TempDir::new().unwrap();
        let payload =
            build_payload_via_main(tmp.path(), "旧内容\n", "旧内容\n\n新增\n", "diff", Some(1));
        assert!(payload.starts_with("<comment on=\"reply[-2]\">"));
    }

    #[test]
    fn test_diff_payload_with_offset_2_targets_indexed_reply() {
        let tmp = TempDir::new().unwrap();
        let payload =
            build_payload_via_main(tmp.path(), "旧内容\n", "旧内容\n\n新增\n", "diff", Some(2));
        assert!(payload.starts_with("<comment on=\"reply[-3]\">"));
    }

    fn write_codex_transcript(path: &Path, commentary_text: &str) {
        write_lines(
            path,
            &[
                serde_json::json!({"type": "session_meta", "payload": {"id": "sess-codex", "cwd": "/repo"}}),
                serde_json::json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-1"}}),
                serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "assistant", "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "真正要编辑的回答"}]}}),
                serde_json::json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-2"}}),
                serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "$cvim"}]}}),
                serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "assistant", "phase": "commentary",
                    "content": [{"type": "output_text", "text": commentary_text}]}}),
            ],
        );
    }

    #[test]
    fn test_diff_payload_menu_selected_skips_codex_commentary_shift() {
        let tmp = TempDir::new().unwrap();
        let transcript = tmp.path().join("codex.jsonl");
        write_codex_transcript(&transcript, "使用 cvim skill 启动外部编辑器。");
        fs::write(
            tmp.path().join("transcript_path"),
            transcript.to_str().unwrap(),
        )
        .unwrap();
        fs::write(tmp.path().join("menu_selected"), "1").unwrap();

        let payload = build_payload_via_main(tmp.path(), "a\n", "a\nb\n", "diff", Some(1));
        assert!(payload.starts_with("<comment on=\"reply[-2]\">"));
    }

    #[test]
    fn test_diff_payload_uses_effective_offset_from_transcript() {
        let tmp = TempDir::new().unwrap();
        let transcript = tmp.path().join("codex.jsonl");
        write_codex_transcript(
            &transcript,
            "使用 `cvim` skill,按要求直接启动外部编辑器助手。",
        );
        fs::write(
            tmp.path().join("transcript_path"),
            transcript.to_str().unwrap(),
        )
        .unwrap();

        let payload = build_payload_via_main(
            tmp.path(),
            "真正要编辑的回答\n",
            "真正要编辑的回答\n\n补一行\n",
            "diff",
            None,
        );
        assert!(payload.starts_with("<comment on=\"reply[-2]\">"));
    }

    // -- difflib parity ------------------------------------------------------

    #[test]
    fn test_unified_diff_matches_python_difflib_golden() {
        // python3 -c 'import difflib; print("".join(difflib.unified_diff(
        //   a.splitlines(True), b.splitlines(True), "", "")))' golden output.
        let a = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let b = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nNINE\nten\neleven\n";
        let joined: String = difflib::unified_diff(a, b).concat();
        let expected = "--- \n+++ \n@@ -1,10 +1,11 @@\n one\n two\n-three\n+THREE\n four\n \
                        five\n six\n seven\n eight\n-nine\n+NINE\n ten\n+eleven\n";
        assert_eq!(joined, expected);
    }

    #[test]
    fn test_unified_diff_no_trailing_newline_concatenates_like_difflib() {
        let a = "alpha\nbeta";
        let b = "alpha\ngamma";
        let joined: String = difflib::unified_diff(a, b).concat();
        assert_eq!(joined, "--- \n+++ \n@@ -1,2 +1,2 @@\n alpha\n-beta+gamma");
    }

    #[test]
    fn test_unified_diff_identical_inputs_yield_nothing() {
        assert!(difflib::unified_diff("same\n", "same\n").is_empty());
    }

    // -- cvim-list / cvim-seed ----------------------------------------------

    #[test]
    fn test_list_main_writes_seeds_menu_and_prints_count() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer A"}]}),
                serde_json::json!({"role": "user", "content": [{"type": "text", "text": "u"}]}),
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer B"}]}),
            ],
        );
        let seeds = tmp.path().join("seeds");
        let menu_json = tmp.path().join("menu.json");
        let args: Vec<String> = [
            session.to_str().unwrap(),
            seeds.to_str().unwrap(),
            menu_json.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(list_main(&args), 0);
        assert_eq!(
            fs::read_to_string(seeds.join("0.md")).unwrap(),
            "answer B\n"
        );
        assert_eq!(
            fs::read_to_string(seeds.join("1.md")).unwrap(),
            "answer A\n"
        );
        let menu: Value = serde_json::from_str(&fs::read_to_string(&menu_json).unwrap()).unwrap();
        let rows = menu.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["offset"], 0);
        assert!(rows[0]["label"].as_str().unwrap().contains("answer B"));
        assert!(rows[0]["label"].as_str().unwrap().starts_with("--:--  "));
        assert!(rows[1]["label"].as_str().unwrap().contains("answer A"));
    }

    #[test]
    fn test_seed_main_writes_last_assistant_message_with_newline() {
        let tmp = TempDir::new().unwrap();
        let session = make_claude_session(
            tmp.path(),
            &[
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "seeded"}]}),
            ],
        );
        let dst = tmp.path().join("message.md");
        let args: Vec<String> = [dst.to_str().unwrap(), session.to_str().unwrap(), "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(seed_main(&args), 0);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "seeded\n");
    }

    #[test]
    fn test_seed_main_blank_without_preferred() {
        let tmp = TempDir::new().unwrap();
        let dst = tmp.path().join("message.md");
        let args: Vec<String> = vec![dst.to_str().unwrap().to_string()];
        assert_eq!(seed_main(&args), 0);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "");
    }

    // -- sendback routing (pure branches) ------------------------------------

    #[test]
    fn test_sendback_unknown_profile_falls_back_to_tmux_keys() {
        let (code, fields) = sendback("%1", "mystery", Some("hello"), false);
        assert_eq!(code, NO_NATIVE_ADDRESS);
        assert_eq!(
            fields,
            vec![
                ("route", "tmuxKeys".to_string()),
                ("why", "profile_mystery".to_string()),
            ]
        );
        let (_, fields) = sendback("%1", "", None, false);
        assert_eq!(fields[1].1, "profile_unknown");
    }

    #[test]
    fn test_sendback_codex_slash_command_routes_to_composer() {
        let (code, fields) = sendback("%1", "codex", Some("/compact"), true);
        assert_eq!(code, NO_NATIVE_ADDRESS);
        assert_eq!(fields[1], ("why", "slash_command".to_string()));
        // Multi-line text starting with "/" is not a slash command.
        assert!(!is_slash_command("/compact\nmore"));
        assert!(is_slash_command("  /compact  "));
    }

    // -- materialization ------------------------------------------------------

    #[test]
    fn test_materialize_assets_writes_and_heals_the_tree() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("HIVE_HOME", tmp.path());
        let command = materialize_assets().unwrap();
        assert!(command.ends_with("core_assets/cvim/bin/cvim-command"));
        assert_eq!(fs::read_to_string(&command).unwrap(), CVIM_COMMAND);
        let mode = fs::metadata(&command).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        let menu = tmp.path().join("core_assets/cvim/resources/menu.vim");
        assert_eq!(fs::read_to_string(&menu).unwrap(), MENU_VIM);

        // Drifted on-disk copy is rewritten to the embedded content.
        fs::write(&command, "stale").unwrap();
        materialize_assets().unwrap();
        assert_eq!(fs::read_to_string(&command).unwrap(), CVIM_COMMAND);
    }
}
