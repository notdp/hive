//! Read-only live mirror for a Claude session transcript.
//!
//! An interactive Claude session (a desktop ccd, a joined session) has no
//! attachable pty — `claude attach` is job-only, and resuming would fork a
//! second engine. Its truth layer is the transcript JSONL, appended event by
//! event as the turn unfolds, so a faithful renderer over that file IS the
//! mirror: native-looking, keystrokes go nowhere by construction.

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
/// groknight palette lifted from xai-grok-pager-render's theme — syntax
/// highlighting, tables, headings, the whole surface, rendered to ANSI.
mod grok_md {
    use std::sync::OnceLock;
    use xai_grok_markdown::{MarkdownStyle, Syntect};

    type Style = anstyle::Style;

    fn rgb(r: u8, g: u8, b: u8) -> anstyle::Color {
        anstyle::Color::Rgb(anstyle::RgbColor(r, g, b))
    }

    fn fg(c: anstyle::Color) -> Style {
        Style::new().fg_color(Some(c))
    }

    // groknight palette (grok-build crates/codegen/xai-grok-pager-render/
    // src/theme/groknight.rs).
    fn style() -> MarkdownStyle {
        let teal = rgb(26, 188, 156);
        let blue = rgb(122, 162, 247);
        let purple = rgb(157, 124, 216);
        let dark5 = rgb(120, 120, 120);
        let comment = rgb(108, 108, 108);
        let dark3 = rgb(90, 90, 90);
        let blue1 = rgb(58, 149, 171);
        let green = rgb(158, 206, 106);
        let fg_dark = rgb(200, 200, 200);
        let link = rgb(122, 166, 218);
        let heading_colors = [teal, blue, purple, dark5, comment, dark3];
        let mut heading_inner = heading_colors.map(fg);
        for s in heading_inner.iter_mut().take(5) {
            *s = s.bold();
        }
        MarkdownStyle {
            heading_inner,
            heading_outer: heading_colors.map(|c| fg(c).dimmed().hidden()),
            strong_inner: fg(fg_dark).bold(),
            strong_outer: Style::new().dimmed().hidden(),
            emphasis_inner: fg(fg_dark).italic(),
            emphasis_outer: Style::new().dimmed().hidden(),
            strikethrough_inner: fg(fg_dark).strikethrough(),
            strikethrough_outer: Style::new().dimmed().hidden(),
            inline_code_inner: fg(blue1).bold(),
            inline_code_outer: fg(blue1).dimmed().hidden(),
            blockquote_outer: fg(comment).dimmed(),
            task_checked: fg(green),
            task_unchecked: fg(fg_dark).dimmed(),
            list_item: fg(comment),
            rule: fg(comment),
            link_outer: fg(comment),
            link_text: fg(link).underline(),
            link_url: fg(comment),
            link_title: fg(comment),
            code_outer: fg(blue1).dimmed().hidden(),
            code_language: fg(purple).hidden(),
            code_untagged: fg(fg_dark),
            code_background: Style::new().bg_color(Some(rgb(28, 28, 28))),
            table_outer: fg(blue).hidden(),
            text: fg(fg_dark),
            math: fg(fg_dark).italic(),
        }
    }

    fn syntect() -> &'static Syntect {
        static SYNTECT: OnceLock<Syntect> = OnceLock::new();
        SYNTECT.get_or_init(|| Syntect::new(include_bytes!("../assets/tokyo-night.tmTheme")))
    }

    /// Render markdown to an ANSI string, trailing whitespace trimmed.
    pub fn render(text: &str) -> String {
        let (out, _) = xai_grok_markdown::render_markdown(text, style(), true, Some(syntect()));
        out.trim_end().to_string()
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
fn hive_envelope(text: &str) -> Option<(&str, &str)> {
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

fn _tool_line(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
    let empty_obj = Value::Object(serde_json::Map::new());
    let inp = match block.get("input") {
        Some(v @ Value::Object(_)) => v,
        _ => &empty_obj,
    };
    // ponytail: hint fields are treated as strings only; Python str()s exotic
    // non-string values, which real transcripts never carry.
    let hint = ["description", "file_path", "command", "prompt"]
        .into_iter()
        .find_map(|k| {
            inp.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| serde_json::to_string(inp).unwrap_or_default());
    let first_line = hint.lines().next().unwrap_or("");
    let hint = _clip(first_line, 140);
    format!("{GREEN}⏺{RESET} {BOLD}{name}{RESET}({CYAN}{hint}{RESET})")
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

/// Fold transcript rows into printed lines and a liveness verdict.
struct Renderer {
    tokens: i64,
    state: &'static str, // idle | working
    state_since: Instant,
}

impl Renderer {
    fn new() -> Self {
        Renderer {
            tokens: 0,
            state: "idle",
            state_since: Instant::now(),
        }
    }

    fn _set_state(&mut self, state: &'static str) {
        if state != self.state {
            self.state = state;
            self.state_since = Instant::now();
        }
    }

    fn render(&mut self, raw: &str) -> Option<String> {
        let row: Value = serde_json::from_str(raw).ok()?;
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            return None;
        }
        let is_user = kind == "user";
        let null = Value::Null;
        let message = row.get("message").unwrap_or(&null);
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
        let mut lines: Vec<String> = Vec::new();
        let mut saw_tool_use = false;
        let mut saw_text = false;
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
                        lines.push(format!("\n{}", _user_line(body)));
                    } else {
                        saw_text = true;
                        lines.push(format!(
                            "\n{}",
                            _indent_block(&_md(&_clip(body, 4000)), "⏺ ", "  ")
                        ));
                    }
                }
                "tool_use" => {
                    saw_tool_use = true;
                    lines.push(format!("\n{}", _tool_line(block)));
                }
                "tool_result" => {
                    let body = block.get("content").unwrap_or(&null);
                    let text = match body.as_str() {
                        Some(s) => s.to_string(),
                        None => serde_json::to_string(body).unwrap_or_default(),
                    };
                    let first = if text.trim().is_empty() {
                        String::new()
                    } else {
                        _clip(&text, 160).lines().next().unwrap_or("").to_string()
                    };
                    if !first.is_empty() {
                        lines.push(format!("  {DIM}⎿  {first}{RESET}"));
                    }
                }
                _ => {}
            }
        }
        if is_user || saw_tool_use {
            self._set_state("working");
        } else if saw_text {
            self._set_state("idle");
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.concat())
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
            self.tokens
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
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("{DIM}── live mirror · {name} · keys go nowhere ──{RESET}");
    let mut renderer = Renderer::new();
    let mut tick: usize = 0;
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
        if let Some(rendered) = renderer.render(raw) {
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
                print!("{}{}", CLEAR_LINE, renderer.status_line(tick, session_id));
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs_f64(_POLL_SECONDS));
            }
            Ok(_) => {
                if let Some(rendered) = renderer.render(&raw) {
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

    fn _row(kind: &str, content: Value, usage: Option<Value>) -> String {
        let mut msg = json!({ "content": content });
        if let Some(u) = usage {
            msg["usage"] = u;
        }
        json!({ "type": kind, "message": msg }).to_string()
    }

    #[test]
    fn test_assistant_text_renders_with_marker_and_markdown() {
        let mut r = Renderer::new();
        let out = r
            .render(&_row(
                "assistant",
                json!([{"type": "text", "text": "done: **all green**"}]),
                None,
            ))
            .unwrap();
        assert!(out.contains("⏺"), "{out}");
        // grok markdown engine: bold content survives, markers are hidden
        assert!(out.contains("all green"), "{out}");
        assert!(!out.contains("**"), "{out}");
        assert_eq!(r.state, "idle");
    }

    #[test]
    fn test_tool_use_prefers_the_human_readable_hint() {
        let mut r = Renderer::new();
        let out = r
            .render(&_row(
                "assistant",
                json!([{"type": "tool_use", "name": "Bash",
                        "input": {"command": "ls", "description": "List files"}}]),
                None,
            ))
            .unwrap();
        assert!(out.contains("Bash") && out.contains("List files"));
        assert!(!out.replace("List files", "").contains("ls"));
        assert_eq!(r.state, "working");
    }

    #[test]
    fn test_hive_envelope_collapses_to_a_tagged_line() {
        let mut r = Renderer::new();
        let body = "<HIVE from=comb.dodo to=comb.rex msgId=a1>review the spec</HIVE>";
        let out = r.render(&_row("user", json!(body), None)).unwrap();
        assert!(out.contains("✉") && out.contains("comb.dodo") && out.contains("review the spec"));
        assert!(!out.contains("<HIVE"));
        assert_eq!(r.state, "working");
    }

    #[test]
    fn test_user_turn_flips_working_and_final_text_flips_idle() {
        let mut r = Renderer::new();
        r.render(&_row("user", json!("hi"), None));
        assert_eq!(r.state, "working");
        r.render(&_row(
            "assistant",
            json!([{"type": "text", "text": "hello"}]),
            None,
        ));
        assert_eq!(r.state, "idle");
    }

    #[test]
    fn test_output_tokens_accumulate_into_the_status_line() {
        let mut r = Renderer::new();
        r.render(&_row(
            "assistant",
            json!([{"type": "text", "text": "a"}]),
            Some(json!({"output_tokens": 40})),
        ));
        r.render(&_row(
            "assistant",
            json!([{"type": "text", "text": "b"}]),
            Some(json!({"output_tokens": 2})),
        ));
        assert!(r.status_line(0, "deadbeef-1234").contains("42 tokens out"));
    }

    #[test]
    fn test_non_message_rows_render_nothing() {
        let mut r = Renderer::new();
        assert!(r.render(&json!({"type": "system"}).to_string()).is_none());
        assert!(r.render("not json").is_none());
    }

}
