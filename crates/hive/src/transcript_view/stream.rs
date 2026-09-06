use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::grok_md;
use super::model::{clip, parse_hive_message, DisplayBlock, ToolOutcome};
use super::parser::TranscriptParser;

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

fn md(text: &str) -> String {
    grok_md::render(text)
}

fn indent_block(text: &str, first: &str, rest: &str) -> String {
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

// ---------------------------------------------------------------------------
// Plain (non-tty) ANSI stream over the block model
// ---------------------------------------------------------------------------

fn tool_line(name: &str, hint: &str) -> String {
    let hint = clip(hint.lines().next().unwrap_or(""), 140);
    format!("{GREEN}⏺{RESET} {BOLD}{name}{RESET}({CYAN}{hint}{RESET})")
}

fn result_line(result: &Option<ToolOutcome>) -> Option<String> {
    let res = result.as_ref()?;
    let first = res.first_line();
    let first = first.trim();
    if first.is_empty() {
        return None;
    }
    Some(format!("\n  {DIM}⎿  {first}{RESET}"))
}

fn user_line(text: &str) -> String {
    if let Some(msg) = parse_hive_message(text) {
        let sender = msg.from.as_deref().unwrap_or("peer");
        let body = clip(&msg.body, 160);
        return format!("{MAGENTA}✉{RESET} {BOLD}{sender}{RESET} {DIM}▸{RESET} {body}");
    }
    let first = format!("{BOLD}❯{RESET} {BOLD}");
    format!("{}{}", indent_block(&clip(text, 1200), &first, "  "), RESET)
}

/// Print [`DisplayBlock`]s as the plain ANSI stream (piped mode).
pub(super) struct StreamPrinter {
    pub(super) parser: TranscriptParser,
    pub(super) working: bool,
    /// When `working` last flipped, for the status line's elapsed counter.
    state_since: Instant,
}

/// Reassembles whole JSONL rows from a follow loop's reads. A read at EOF
/// hands back whatever bytes have landed so far — possibly cut inside a
/// multi-byte char — and a row the parser cannot decode is dropped for
/// good. So the accumulator owns raw bytes, releases a row only once its
/// `'\n'` arrives, and decodes it only then.
#[derive(Default)]
pub struct LineAccumulator {
    partial: Vec<u8>,
}

impl LineAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one `read_until(b'\n')` result; returns the row it completed,
    /// newline stripped, or `None` while the row is still partial.
    pub fn push(&mut self, chunk: &[u8]) -> Option<String> {
        self.partial.extend_from_slice(chunk);
        if self.partial.last() != Some(&b'\n') {
            return None;
        }
        let mut bytes = std::mem::take(&mut self.partial);
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Split an initial-load backlog into its whole rows; a trailing row
    /// without `'\n'` is held back until the follow loop reads the rest.
    pub fn split_backlog(&mut self, backlog: &[u8]) -> Vec<String> {
        let cut = backlog
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        let (whole, rest) = backlog.split_at(cut);
        self.partial.extend_from_slice(rest);
        String::from_utf8_lossy(whole)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl StreamPrinter {
    pub(super) fn new() -> Self {
        StreamPrinter {
            parser: TranscriptParser::new(),
            working: false,
            state_since: Instant::now(),
        }
    }

    fn sync_state(&mut self) {
        let working = self.parser.busy();
        if working != self.working {
            self.working = working;
            self.state_since = Instant::now();
        }
    }

    pub(super) fn push_rendered(&mut self, raw: &str) -> Option<String> {
        let blocks = self.parser.push(raw);
        self.sync_state();
        Self::render_blocks(&blocks)
    }

    pub(super) fn flush_rendered(&mut self) -> Option<String> {
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
            DisplayBlock::User(u) => Some(format!("\n{}", user_line(&u.text))),
            DisplayBlock::Assistant(a) => Some(format!(
                "\n{}",
                indent_block(&md(&clip(&a.markdown, 4000)), "⏺ ", "  ")
            )),
            DisplayBlock::ToolGroup(group) => {
                let mut out = String::new();
                for member in &group.members {
                    out.push('\n');
                    out.push_str(&tool_line(&member.name, &member.hint));
                    if let Some(res) = result_line(&member.result) {
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
                let mut out = format!("\n{}", tool_line("Bash", &run.description));
                if let Some(res) = result_line(&run.result) {
                    out.push_str(&res);
                }
                Some(out)
            }
            DisplayBlock::Tool(tool) => {
                let mut out = format!("\n{}", tool_line(&tool.name, &tool.hint));
                if let Some(res) = result_line(&tool.result) {
                    out.push_str(&res);
                }
                Some(out)
            }
            // The plain stream never showed thinking or turn markers.
            DisplayBlock::Thinking(_) | DisplayBlock::WorkedFor(_) => None,
        }
    }

    pub(super) fn status_line(&self, tick: usize, session_id: &str) -> String {
        let verb = if self.working {
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

/// Plain ANSI stream of the transcript at *path*, followed live: the
/// non-tty form of `hive view` (pipes, logs).
pub fn follow_plain(session_id: &str, path: &Path) -> i32 {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("{DIM}── live mirror · {name} · keys go nowhere ──{RESET}");
    let mut printer = StreamPrinter::new();
    let mut tick: usize = 0;
    let mut idle_ticks: usize = 0;
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("{}: {}", path.display(), err);
            return 1;
        }
    };
    let mut reader = BufReader::new(file);
    let mut backlog = Vec::new();
    if let Err(err) = reader.read_to_end(&mut backlog) {
        eprintln!("{}: {}", path.display(), err);
        return 1;
    }
    let mut lines = LineAccumulator::new();
    let whole = lines.split_backlog(&backlog);
    for raw in &whole[whole.len().saturating_sub(_TAIL_EVENTS)..] {
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
    let mut raw = Vec::new();
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            print!("{CLEAR_LINE}");
            let _ = std::io::stdout().flush();
            return 0;
        }
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
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
                let Some(line) = lines.push(&raw) else {
                    continue;
                };
                idle_ticks = 0;
                if let Some(rendered) = printer.push_rendered(&line) {
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
