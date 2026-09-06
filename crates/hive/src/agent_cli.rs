//! Agent CLI profiles: claude, codex, grok.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::adapters::base::SessionAdapter;
use crate::tmux::TTYProcessInfo;

pub const AGENT_CLI_NAMES: [&str; 3] = ["claude", "codex", "grok"];

pub const SHELL_NAMES: [&str; 8] = ["zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh"];

// macOS Claude Code reports its process comm as "claude.exe"; without this
// the command probe misses and detection falls back to the pane title,
// which misclassifies a claude pane whose title happens to contain another
// CLI's name (e.g. "Research Codex app server" -> codex).
const CLI_ALIASES: [(&str, &str); 3] = [
    ("claude-code", "claude"),
    ("claudecode", "claude"),
    ("claude.exe", "claude"),
];

/// Model ids from the CLI's own backend catalog on disk; [] = no catalog.
///
/// codex and grok refresh these caches from their backends themselves, so
/// the list never drifts the way a hand-maintained table did. claude keeps
/// no local catalog — its aliases and ids are validated by the CLI itself.
fn catalog_model_ids(cli: &str) -> Vec<String> {
    let load = |path: PathBuf| -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    };
    if cli == "codex" {
        let Some(data) =
            load(crate::adapters::codex_app_server::codex_home().join("models_cache.json"))
        else {
            return Vec::new();
        };
        let Some(models) = data.get("models").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        return models
            .iter()
            .filter_map(|m| m.get("slug"))
            .filter_map(|slug| slug.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
    }
    if cli == "grok" {
        let Some(data) = load(crate::adapters::grok_leader::grok_home().join("models_cache.json"))
        else {
            return Vec::new();
        };
        let Some(models) = data.get("models").and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        return models.keys().cloned().collect();
    }
    Vec::new()
}

fn cli_family(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("anthropic"),
        "codex" => Some("openai"),
        "grok" => Some("xai"),
        _ => None,
    }
}

/// Error string when *model* is surely wrong for *cli*, else None.
///
/// Two gates: a cross-family check (a gpt model handed to claude is always
/// a mistake, catalog or not), then the CLI's own catalog when one exists
/// on disk. No catalog or an unreadable cache fails open — the CLI is the
/// final authority and its own rejection is visible in the pane (claude
/// keeps no local catalog but rejects unknown ids itself at launch).
pub fn validate_spawn_model(cli: &str, model: &str) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    let family = classify_model_family(model);
    let cli_family = cli_family(cli);
    if family != "unknown" {
        if let Some(cli_family) = cli_family {
            if family != cli_family {
                return Some(format!(
                    "model '{model}' is a {family} model, but {cli} runs \
{cli_family} models — wrong --cli or wrong -m"
                ));
            }
        }
    }
    let known = catalog_model_ids(cli);
    if known.is_empty() || known.iter().any(|k| k == model) {
        return None;
    }
    let hint = match get_close_match(model, &known) {
        Some(close) => format!(" (did you mean '{close}'?)"),
        None => String::new(),
    };
    Some(format!(
        "unknown {cli} model '{model}'{hint}; its catalog has: {}",
        known.join(", ")
    ))
}

/// Classify a model identifier into a coarse family for peer diversity.
///
/// Returns 'anthropic', 'openai', 'xai', or 'unknown'.
pub fn classify_model_family(model: &str) -> &'static str {
    if model.is_empty() {
        return "unknown";
    }
    let m = model.to_lowercase();
    let m = m.trim().trim_start_matches('-');
    if m.contains("claude")
        || ["opus", "sonnet", "haiku", "fable"]
            .iter()
            .any(|p| m.starts_with(p))
    {
        return "anthropic";
    }
    if m.contains("codex") || ["gpt", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p)) {
        return "openai";
    }
    if m.contains("grok") {
        return "xai";
    }
    "unknown"
}

pub fn normalize_command(command: &str) -> String {
    let value = command.trim().to_lowercase();
    let value = value.rsplit('/').next().unwrap_or("");
    let value = value.trim_start_matches('-');
    for (alias, name) in CLI_ALIASES {
        if value == alias {
            return name.to_string();
        }
    }
    value.to_string()
}

pub fn is_agent_command(command: &str) -> bool {
    AGENT_CLI_NAMES.contains(&normalize_command(command).as_str())
}

pub fn is_shell_command(command: &str) -> bool {
    SHELL_NAMES.contains(&normalize_command(command).as_str())
}

pub fn member_role(command: &str) -> &'static str {
    if is_agent_command(command) {
        "agent"
    } else {
        "terminal"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CLIProfile {
    pub name: &'static str,
    pub fork_cmd: &'static str,
    pub skill_cmd: &'static str,
}

impl CLIProfile {
    /// Python `profile.fork_cmd.format(session_id=...)`.
    pub fn fork_cmd_for(&self, session_id: &str) -> String {
        self.fork_cmd.replace("{session_id}", session_id)
    }

    /// Python `profile.skill_cmd.format(name=...)`.
    pub fn skill_cmd_for(&self, name: &str) -> String {
        self.skill_cmd.replace("{name}", name)
    }
}

pub static PROFILES: [CLIProfile; 3] = [
    CLIProfile {
        name: "claude",
        fork_cmd: "hive claude -r {session_id} --fork-session",
        skill_cmd: "/{name}",
    },
    CLIProfile {
        name: "codex",
        fork_cmd: "hive codex fork {session_id}",
        skill_cmd: "${name}",
    },
    CLIProfile {
        name: "grok",
        fork_cmd: "hive grok --resume {session_id} --fork-session",
        skill_cmd: "/{name}",
    },
];

fn profile_by_name(name: &str) -> Option<&'static CLIProfile> {
    PROFILES.iter().find(|p| p.name == name)
}

pub fn get_profile(command: &str) -> Option<&'static CLIProfile> {
    profile_by_name(&normalize_command(command))
}

pub fn detect_profile_from_text(text: &str) -> Option<&'static CLIProfile> {
    let value = text.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    if value.contains("claude code") {
        return profile_by_name("claude");
    }
    for (alias, profile_name) in CLI_ALIASES {
        if value.contains(alias) {
            return profile_by_name(profile_name);
        }
    }
    PROFILES.iter().find(|p| value.contains(p.name))
}

// Script runtimes whose argv[1] is the launched CLI's entry script — the one
// verified wrapper shape (codex runs as `node /.../codex ...`). Anything else
// in argv is ordinary argument text and must never identify a CLI.
const _SCRIPT_RUNTIMES: [&str; 1] = ["node"];

/// CLI identity from process fields, not argument text.
///
/// Matches the executable itself (ps comm / `argv[0]`) or the verified script
/// runtime shape `node <.../codex|claude> ...`. Later argv tokens are the
/// process's own arguments — `rg codex src` is a search, not a CLI — so
/// they are never scanned.
pub fn detect_profile_from_process(command: &str, argv: &str) -> Option<&'static CLIProfile> {
    if let Some(profile) = get_profile(command) {
        return Some(profile);
    }
    let parts = shlex_split(argv);
    if parts.is_empty() {
        return None;
    }
    if let Some(profile) = get_profile(&parts[0]) {
        return Some(profile);
    }
    if parts.len() >= 2 && _SCRIPT_RUNTIMES.contains(&normalize_command(&parts[0]).as_str()) {
        return get_profile(&parts[1]);
    }
    None
}

/// Python `shlex.split(argv)` with the whitespace-split fallback the Python
/// code uses on `ValueError` (unbalanced quote / trailing escape).
fn shlex_split(argv: &str) -> Vec<String> {
    match posix_shlex(argv) {
        Ok(parts) => parts,
        Err(()) => argv.split_whitespace().map(str::to_string).collect(),
    }
}

/// POSIX-mode shlex: single quotes literal, double quotes with `\"`/`\\`
/// escapes, bare backslash escapes the next char. Err = Python ValueError.
fn posix_shlex(s: &str) -> Result<Vec<String>, ()> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if has_token {
                    parts.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(()),
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('\\' | '"')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => return Err(()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(()),
                    }
                }
            }
            '\\' => {
                has_token = true;
                match chars.next() {
                    Some(n) => cur.push(n),
                    None => return Err(()),
                }
            }
            ch => {
                has_token = true;
                cur.push(ch);
            }
        }
    }
    if has_token {
        parts.push(cur);
    }
    Ok(parts)
}

// --- pane probes (test-overridable seams onto tmux) -------------------------

#[cfg(test)]
mod test_hooks {
    use std::cell::RefCell;

    use crate::adapters::base::SessionAdapter;
    use crate::tmux::TTYProcessInfo;

    /// Stand-in for the Python tests' monkeypatched `hive.agent_cli.tmux.*`.
    #[derive(Default)]
    pub struct PaneProbes {
        pub current_command: Option<String>,
        pub title: Option<String>,
        pub tty: Option<String>,
        pub processes: Vec<TTYProcessInfo>,
        pub display: Option<String>,
    }

    thread_local! {
        pub static PANE_PROBES: RefCell<Option<PaneProbes>> = const { RefCell::new(None) };
        #[allow(clippy::type_complexity)]
        pub static ADAPTER_GET: RefCell<
            Option<Box<dyn Fn(&str) -> Option<Box<dyn SessionAdapter>>>>,
        > = const { RefCell::new(None) };
    }
}

#[cfg(test)]
fn probe<T>(f: impl Fn(&test_hooks::PaneProbes) -> T) -> Option<T> {
    test_hooks::PANE_PROBES.with(|p| p.borrow().as_ref().map(f))
}

fn pane_current_command(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = probe(|p| p.current_command.clone()) {
            return v;
        }
    }
    crate::tmux::get_pane_current_command(pane_id)
}

fn pane_title(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = probe(|p| p.title.clone()) {
            return v;
        }
    }
    crate::tmux::get_pane_title(pane_id)
}

fn pane_tty(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = probe(|p| p.tty.clone()) {
            return v;
        }
    }
    crate::tmux::get_pane_tty(pane_id)
}

fn tty_processes(tty: &str) -> Vec<TTYProcessInfo> {
    #[cfg(test)]
    {
        if let Some(v) = probe(|p| p.processes.clone()) {
            return v;
        }
    }
    crate::tmux::list_tty_processes(tty)
}

fn pane_display_value(pane_id: &str, fmt: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(v) = probe(|p| p.display.clone()) {
            return v;
        }
    }
    crate::tmux::display_value(pane_id, fmt)
}

/// Python `adapters.get(name)` (the registry in adapters/__init__.py).
fn adapter_for(name: &str) -> Option<Box<dyn SessionAdapter>> {
    #[cfg(test)]
    {
        let hit = test_hooks::ADAPTER_GET.with(|g| g.borrow().as_ref().map(|f| f(name)));
        if let Some(v) = hit {
            return v;
        }
    }
    match name {
        "claude" => Some(Box::new(crate::adapters::claude::ClaudeAdapter)),
        "codex" => Some(Box::new(crate::adapters::codex::CodexAdapter)),
        "grok" => Some(Box::new(crate::adapters::grok::GrokAdapter)),
        _ => None,
    }
}

/// CLI profile from live agent evidence only — never the pane title.
///
/// A retained shell keeps the pane (and often a stale title naming a CLI)
/// after the agent process exits, so title text must not count as liveness
/// evidence. Evidence is the pane's current command and its TTY process
/// table, parsed by the same matchers as `detect_profile_for_pane` — plus,
/// for claude, the pane's bg job record: a claude member's engine runs on
/// claude's own supervisor, and the pane only shows it through an attach
/// viewer, so a viewer gap (reattach window, closed viewer) with a live
/// engine still counts as a live claude. Any probe failure fails closed to
/// None.
pub fn detect_cli_process_for_pane(pane_id: &str) -> Option<&'static CLIProfile> {
    let profile = get_profile(&pane_current_command(pane_id).unwrap_or_default());
    if profile.is_some() {
        return profile;
    }
    let tty = pane_tty(pane_id).unwrap_or_default();
    for process in tty_processes(&tty) {
        if let Some(profile) = detect_profile_from_process(&process.command, &process.argv) {
            return Some(profile);
        }
    }
    if crate::adapters::claude_bg::pane_engine_alive(pane_id) {
        return profile_by_name("claude");
    }
    None
}

/// Pid of the live claude process on *pane_id*'s tty (process evidence
/// only, same matchers as `detect_cli_process_for_pane`).
///
/// On a bg-member pane this is the attach *viewer*'s pid — never member
/// identity or delivery routing (both key on the pane's job record). It
/// answers only tty-scoped questions: is there a viewer to keystroke into,
/// or an interactive (non-member) claude session on this pane.
pub fn claude_pid_for_pane(pane_id: &str) -> Option<i32> {
    let tty = pane_tty(pane_id).unwrap_or_default();
    for process in tty_processes(&tty) {
        if let Some(profile) = detect_profile_from_process(&process.command, &process.argv) {
            if profile.name == "claude" {
                return process.pid.trim().parse::<i32>().ok();
            }
        }
    }
    None
}

pub fn detect_profile_for_pane(pane_id: &str) -> Option<&'static CLIProfile> {
    if let Some(profile) = detect_cli_process_for_pane(pane_id) {
        return Some(profile);
    }
    detect_profile_from_text(&pane_title(pane_id).unwrap_or_default())
}

pub fn member_role_for_pane(pane_id: &str) -> &'static str {
    if detect_profile_for_pane(pane_id).is_some() {
        return "agent";
    }
    // A pane bound to a bg job is an agent pane even while its engine is
    // parked (asleep is not dead): the lead's role — and with it runtime
    // ticks and idle notify — must not ride the viewer's life.
    if crate::adapters::claude_bg::job_id_for_pane(pane_id).is_some() {
        return "agent";
    }
    member_role(&pane_current_command(pane_id).unwrap_or_default())
}

pub fn resolve_session_id_for_pane(pane_id: &str, profile: Option<&CLIProfile>) -> Option<String> {
    let resolved_name = match profile {
        Some(p) => p.name,
        None => detect_profile_for_pane(pane_id)?.name,
    };
    let adapter = adapter_for(resolved_name)?;
    adapter.resolve_current_session_id(pane_id)
}

pub fn resolve_model_for_pane(pane_id: &str, cli_name: &str, current_model: &str) -> String {
    let profile = if cli_name.is_empty() {
        detect_profile_for_pane(pane_id)
    } else {
        get_profile(cli_name)
    };
    let Some(profile) = profile else {
        return current_model.to_string();
    };
    let Some(adapter) = adapter_for(profile.name) else {
        return current_model.to_string();
    };
    let session_id = match adapter.resolve_current_session_id(pane_id) {
        Some(s) if !s.is_empty() => s,
        _ => return current_model.to_string(),
    };
    let cwd_hint = pane_display_value(pane_id, "#{pane_current_path}");
    let Some(transcript) = adapter.find_session_file(&session_id, cwd_hint.as_deref()) else {
        return current_model.to_string();
    };
    match adapter.read_meta(&transcript) {
        Some(meta) => match meta.model {
            Some(model) if !model.is_empty() => model,
            _ => current_model.to_string(),
        },
        None => current_model.to_string(),
    }
}

// --- difflib.get_close_matches(word, known, n=1, cutoff=0.4) ----------------

/// SequenceMatcher find_longest_match (no junk, no autojunk: model ids are
/// far under the 200-char autojunk threshold).
fn find_longest(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut newj2len: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(ch) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j2len.get(&j.wrapping_sub(1)).copied().unwrap_or(0) + 1;
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
    (besti, bestj, bestsize)
}

fn match_total(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> usize {
    let (i, j, k) = find_longest(a, b2j, alo, ahi, blo, bhi);
    if k == 0 {
        return 0;
    }
    k + match_total(a, b2j, alo, i, blo, j) + match_total(a, b2j, i + k, ahi, j + k, bhi)
}

/// SequenceMatcher(None, a, b).ratio().
fn seq_ratio(a: &[char], b: &[char]) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, &ch) in b.iter().enumerate() {
        b2j.entry(ch).or_default().push(j);
    }
    let matches = match_total(a, &b2j, 0, a.len(), 0, b.len());
    2.0 * matches as f64 / total as f64
}

/// difflib.get_close_matches(word, possibilities, n=1, cutoff=0.4), first
/// element only. Ties break like heapq.nlargest on (ratio, string) tuples.
fn get_close_match(word: &str, possibilities: &[String]) -> Option<String> {
    let b: Vec<char> = word.chars().collect();
    let mut best: Option<(f64, &String)> = None;
    for x in possibilities {
        let a: Vec<char> = x.chars().collect();
        let ratio = seq_ratio(&a, &b);
        if ratio < 0.4 {
            continue;
        }
        let better = match best {
            None => true,
            Some((best_ratio, best_x)) => {
                ratio > best_ratio || (ratio == best_ratio && x.as_str() > best_x.as_str())
            }
        };
        if better {
            best = Some((ratio, x));
        }
    }
    best.map(|(_, x)| x.clone())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use crate::testenv::EnvGuard;

    use serde_json::{json, Map, Value};

    use super::test_hooks::{PaneProbes, ADAPTER_GET, PANE_PROBES};
    use super::*;
    use crate::adapters::base::{Message, SessionMeta};

    fn set_probes(probes: PaneProbes) {
        PANE_PROBES.with(|p| *p.borrow_mut() = Some(probes));
    }

    fn set_adapter_get<F>(f: F)
    where
        F: Fn(&str) -> Option<Box<dyn SessionAdapter>> + 'static,
    {
        ADAPTER_GET.with(|g| *g.borrow_mut() = Some(Box::new(f)));
    }

    /// Points the claude_bg job-record probes at a disposable CLAUDE_HOME
    /// for the test's lifetime.
    fn isolate_claude_home() -> (tempfile::TempDir, EnvGuard) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("CLAUDE_HOME", dir.path());
        (dir, env)
    }

    fn proc(pid: &str, command: &str, argv: &str) -> TTYProcessInfo {
        TTYProcessInfo {
            pid: pid.to_string(),
            command: command.to_string(),
            argv: argv.to_string(),
        }
    }

    #[test]
    fn test_normalize_command_strips_path_and_aliases() {
        assert_eq!(normalize_command("/usr/local/bin/claude"), "claude");
        assert_eq!(normalize_command("claude-code"), "claude");
        assert_eq!(normalize_command("CODEX"), "codex");
        assert_eq!(normalize_command("claude.exe"), "claude");
        assert_eq!(normalize_command("/opt/homebrew/bin/claude.exe"), "claude");
        assert_eq!(normalize_command(""), "");
    }

    #[test]
    fn test_member_role_classifies_agents_and_shells() {
        assert_eq!(member_role("claude"), "agent");
        assert_eq!(member_role("codex"), "agent");
        assert_eq!(member_role("grok"), "agent");
        assert_eq!(member_role("zsh"), "terminal");
        assert_eq!(member_role("python3"), "terminal");
    }

    #[test]
    fn test_profiles_use_expected_skill_commands() {
        assert_eq!(get_profile("claude").unwrap().skill_cmd, "/{name}");
        assert_eq!(get_profile("codex").unwrap().skill_cmd, "${name}");
        assert_eq!(get_profile("grok").unwrap().skill_cmd, "/{name}");
    }

    #[test]
    fn test_grok_profile_forks_through_the_hive_launcher() {
        let profile = get_profile("/usr/local/bin/grok").unwrap();
        assert_eq!(profile.name, "grok");
        assert_eq!(
            profile.fork_cmd_for("sess-1"),
            "hive grok --resume sess-1 --fork-session"
        );
    }

    #[test]
    fn test_detect_profile_for_pane_uses_title_and_tty_processes() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("2.1.89".into()),
            title: Some("\u{2733} Claude Code".into()),
            tty: Some("/dev/ttys012".into()),
            processes: vec![],
            ..Default::default()
        });

        let profile = detect_profile_for_pane("%138");

        assert_eq!(profile.map(|p| p.name), Some("claude"));
    }

    #[test]
    fn test_detect_profile_for_pane_falls_back_to_tty_processes() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("2.1.89".into()),
            title: Some("".into()),
            tty: Some("/dev/ttys012".into()),
            processes: vec![proc("100", "-zsh", "-zsh"), proc("200", "codex", "codex")],
            ..Default::default()
        });

        let profile = detect_profile_for_pane("%141");

        assert_eq!(profile.map(|p| p.name), Some("codex"));
    }

    #[test]
    fn test_detect_profile_for_pane_finds_grok_on_the_tty() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("1.0.5".into()),
            title: Some("".into()),
            tty: Some("/dev/ttys012".into()),
            processes: vec![
                proc("100", "-zsh", "-zsh"),
                proc(
                    "200",
                    "grok",
                    "grok --leader --leader-socket /home/.grok/hive/p19.sock",
                ),
            ],
            ..Default::default()
        });

        let profile = detect_profile_for_pane("%19");

        assert_eq!(profile.map(|p| p.name), Some("grok"));
    }

    #[test]
    fn test_detect_profile_for_pane_reads_codex_argv_without_claude_path_false_positive() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("node".into()),
            title: Some("".into()),
            tty: Some("/dev/ttys012".into()),
            processes: vec![proc(
                "200",
                "node",
                "node /opt/homebrew/bin/codex --cd /repo/.claude/worktrees/feature",
            )],
            ..Default::default()
        });

        let profile = detect_profile_for_pane("%141");

        assert_eq!(profile.map(|p| p.name), Some("codex"));
    }

    #[test]
    fn test_detect_profile_for_pane_claude_exe_not_misled_by_codex_title() {
        // Regression: macOS Claude Code reports comm "claude.exe". The command
        // probe must resolve it to claude so detection never falls back to the
        // pane title, which would misclassify a claude pane whose title
        // mentions another CLI.
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("claude.exe".into()),
            title: Some("✳ Research Codex app server".into()),
            tty: Some("/dev/ttys012".into()),
            processes: vec![],
            ..Default::default()
        });

        let profile = detect_profile_for_pane("%1");

        assert_eq!(profile.map(|p| p.name), Some("claude"));
    }

    struct FakeAdapter {
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl SessionAdapter for FakeAdapter {
        fn name(&self) -> &'static str {
            "claude"
        }
        fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
            self.calls.borrow_mut().push(pane_id.to_string());
            Some("fake-sess".to_string())
        }
        fn find_session_file(&self, _session_id: &str, _cwd: Option<&str>) -> Option<PathBuf> {
            unreachable!()
        }
        fn read_meta(&self, _path: &Path) -> Option<SessionMeta> {
            unreachable!()
        }
        fn iter_messages(&self, _path: &Path) -> Box<dyn Iterator<Item = Message>> {
            unreachable!()
        }
    }

    #[test]
    fn test_resolve_session_id_for_pane_dispatches_to_adapter() {
        set_probes(PaneProbes {
            current_command: Some("claude".into()),
            title: Some("".into()),
            tty: Some("".into()),
            processes: vec![],
            ..Default::default()
        });

        let calls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let calls_hook = calls.clone();
        set_adapter_get(move |name| {
            if name == "claude" {
                Some(Box::new(FakeAdapter {
                    calls: calls_hook.clone(),
                }) as Box<dyn SessionAdapter>)
            } else {
                None
            }
        });

        assert_eq!(
            resolve_session_id_for_pane("%138", None),
            Some("fake-sess".to_string())
        );
        assert_eq!(*calls.borrow(), vec!["%138".to_string()]);
    }

    #[test]
    fn test_resolve_session_id_for_pane_returns_none_when_no_profile() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("zsh".into()),
            title: Some("".into()),
            tty: Some("".into()),
            processes: vec![],
            ..Default::default()
        });

        assert_eq!(resolve_session_id_for_pane("%2", None), None);
    }

    struct FakeCodexAdapter {
        transcript: Option<PathBuf>,
    }

    impl SessionAdapter for FakeCodexAdapter {
        fn name(&self) -> &'static str {
            "codex"
        }
        fn resolve_current_session_id(&self, _pane_id: &str) -> Option<String> {
            // daemon-first resolution happens inside the adapter
            self.transcript.as_ref().map(|_| "sess-app".to_string())
        }
        fn find_session_file(&self, session_id: &str, _cwd: Option<&str>) -> Option<PathBuf> {
            if session_id == "sess-app" {
                self.transcript.clone()
            } else {
                None
            }
        }
        fn read_meta(&self, _path: &Path) -> Option<SessionMeta> {
            Some(SessionMeta {
                session_id: "sess-app".to_string(),
                cwd: None,
                model: Some("gpt-5.5".to_string()),
            })
        }
        fn iter_messages(&self, _path: &Path) -> Box<dyn Iterator<Item = Message>> {
            unreachable!()
        }
    }

    #[test]
    fn test_resolve_model_for_pane_reads_model_from_adapter_session() {
        // resolve_model_for_pane reads the model from whatever session the
        // adapter resolves. For codex the daemon-first lookup lives inside the
        // adapter, so the fake adapter here simply hands back a session id.
        let dir = tempfile::TempDir::new().unwrap();
        let transcript = dir.path().join("rollout.jsonl");
        std::fs::write(&transcript, "").unwrap();

        let transcript_hook = transcript.clone();
        set_adapter_get(move |name| {
            if name == "codex" {
                Some(Box::new(FakeCodexAdapter {
                    transcript: Some(transcript_hook.clone()),
                }) as Box<dyn SessionAdapter>)
            } else {
                None
            }
        });
        set_probes(PaneProbes {
            display: Some("/work".into()),
            ..Default::default()
        });

        assert_eq!(resolve_model_for_pane("%1", "codex", ""), "gpt-5.5");
    }

    #[test]
    fn test_resolve_model_for_pane_no_session_returns_current() {
        // When the adapter resolves no session (e.g. embedded codex with
        // nothing open), resolve_model_for_pane keeps the caller's default.
        set_adapter_get(|name| {
            if name == "codex" {
                Some(Box::new(FakeCodexAdapter { transcript: None }) as Box<dyn SessionAdapter>)
            } else {
                None
            }
        });

        assert_eq!(resolve_model_for_pane("%9", "codex", ""), "");
    }

    #[test]
    fn test_member_role_for_pane_returns_agent_when_profile_detected() {
        set_probes(PaneProbes {
            current_command: Some("codex".into()),
            title: Some("".into()),
            tty: Some("".into()),
            processes: vec![],
            ..Default::default()
        });

        assert_eq!(member_role_for_pane("%1"), "agent");
    }

    #[test]
    fn test_member_role_for_pane_returns_terminal_for_shell() {
        let _home = isolate_claude_home();
        set_probes(PaneProbes {
            current_command: Some("zsh".into()),
            title: Some("".into()),
            tty: Some("".into()),
            processes: vec![],
            ..Default::default()
        });

        assert_eq!(member_role_for_pane("%2"), "terminal");
    }

    // --- strict process matcher: argument text is never CLI identity ---

    #[test]
    fn test_process_matcher_rejects_cli_names_in_arguments() {
        // ordinary shell commands mentioning a CLI name must not read as a CLI
        assert!(detect_profile_from_process("rg", "rg codex src tests").is_none());
        assert!(detect_profile_from_process("git", "git grep claude").is_none());
        assert!(detect_profile_from_process("python", "python script.py codex").is_none());
        // a non-runtime argv[0] with a CLI-named script arg is not the wrapper shape
        assert!(detect_profile_from_process("node", "node script.js codex").is_none());
    }

    #[test]
    fn test_process_matcher_accepts_executable_and_node_wrapper() {
        assert_eq!(
            detect_profile_from_process("claude", "claude --verbose")
                .unwrap()
                .name,
            "claude"
        );
        assert_eq!(
            detect_profile_from_process("claude.exe", "").unwrap().name,
            "claude"
        );
        assert_eq!(
            detect_profile_from_process("node", "node /opt/homebrew/bin/codex --remote unix:///s")
                .unwrap()
                .name,
            "codex"
        );
        // argv[0] identity works even when ps comm is generic
        assert_eq!(
            detect_profile_from_process("something", "/usr/local/bin/claude --continue")
                .unwrap()
                .name,
            "claude"
        );
        assert_eq!(
            detect_profile_from_process("grok", "grok --leader-socket /s")
                .unwrap()
                .name,
            "grok"
        );
        assert_eq!(
            detect_profile_from_process("1.0.5", "/opt/homebrew/bin/grok --resume s")
                .unwrap()
                .name,
            "grok"
        );
        // a grok mention in argument text is still not a CLI
        assert!(detect_profile_from_process("rg", "rg grok src").is_none());
    }

    #[test]
    fn test_claude_pid_for_pane_returns_the_claude_process_pid() {
        set_probes(PaneProbes {
            tty: Some("/dev/ttys012".into()),
            processes: vec![
                proc("123", "-zsh", "-zsh"),
                proc("456", "claude", "claude --model x"),
            ],
            ..Default::default()
        });
        assert_eq!(claude_pid_for_pane("%1"), Some(456));
    }

    #[test]
    fn test_claude_pid_for_pane_ignores_non_claude_processes() {
        // argv mentions of "claude" (rg, git grep) must not bind a pid: the
        // same process-identity rule the retained-shell probe uses
        set_probes(PaneProbes {
            tty: Some("/dev/ttys012".into()),
            processes: vec![
                proc("123", "-zsh", "-zsh"),
                proc("9", "rg", "rg claude src"),
            ],
            ..Default::default()
        });
        assert_eq!(claude_pid_for_pane("%1"), None);
        set_probes(PaneProbes {
            tty: Some("".into()),
            processes: vec![
                proc("123", "-zsh", "-zsh"),
                proc("9", "rg", "rg claude src"),
            ],
            ..Default::default()
        });
        assert_eq!(claude_pid_for_pane("%1"), None);
    }

    #[test]
    fn test_classify_model_family_reads_grok_models_as_xai() {
        assert_eq!(classify_model_family("grok-4.6"), "xai");
        assert_eq!(classify_model_family("grok-build"), "xai");
        assert_eq!(classify_model_family("claude-opus-4-8"), "anthropic");
        assert_eq!(classify_model_family("gpt-5.5"), "openai");
    }

    // --- spawn model validation ---------------------------------------------

    fn codex_cache(root: &Path, slugs: &[&str]) -> PathBuf {
        let home = root.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let models: Vec<Value> = slugs.iter().map(|s| json!({ "slug": s })).collect();
        std::fs::write(
            home.join("models_cache.json"),
            serde_json::to_string(&json!({ "models": models })).unwrap(),
        )
        .unwrap();
        home
    }

    fn grok_cache(root: &Path, ids: &[&str]) -> PathBuf {
        let home = root.join("grok");
        std::fs::create_dir_all(&home).unwrap();
        let mut models = Map::new();
        for id in ids {
            models.insert(id.to_string(), json!({}));
        }
        std::fs::write(
            home.join("models_cache.json"),
            serde_json::to_string(&json!({ "models": models })).unwrap(),
        )
        .unwrap();
        home
    }

    #[test]
    fn test_validate_spawn_model_accepts_a_catalog_hit() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = codex_cache(dir.path(), &["gpt-5.6-sol", "gpt-5.5"]);
        let mut env = EnvGuard::new();
        env.set("CODEX_HOME", &home);
        assert_eq!(validate_spawn_model("codex", "gpt-5.6-sol"), None);
    }

    #[test]
    fn test_validate_spawn_model_rejects_a_miss_with_a_hint() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = codex_cache(dir.path(), &["gpt-5.6-sol", "gpt-5.5"]);
        let mut env = EnvGuard::new();
        env.set("CODEX_HOME", &home);
        let error = validate_spawn_model("codex", "gpt-5.6-sole").expect("error");
        assert!(error.contains("gpt-5.6-sole") && error.contains("gpt-5.6-sol"));
    }

    #[test]
    fn test_validate_spawn_model_reads_the_grok_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = grok_cache(dir.path(), &["grok-4.6", "grok-4.5"]);
        let mut env = EnvGuard::new();
        env.set("GROK_HOME", &home);
        assert_eq!(validate_spawn_model("grok", "grok-4.6"), None);
        assert!(validate_spawn_model("grok", "grok-build").is_some());
    }

    #[test]
    fn test_validate_spawn_model_fails_open_without_a_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("CODEX_HOME", dir.path().join("empty"));
        env.set("GROK_HOME", dir.path().join("empty"));
        assert_eq!(validate_spawn_model("codex", "gpt-anything"), None);
        assert_eq!(validate_spawn_model("grok", "grok-anything"), None);
        // claude keeps no local catalog: always the CLI's own call
        assert_eq!(validate_spawn_model("claude", "claude-nope"), None);
    }

    #[test]
    fn test_validate_spawn_model_ignores_empty_model() {
        assert_eq!(validate_spawn_model("codex", ""), None);
    }

    #[test]
    fn test_validate_spawn_model_refuses_cross_family_mistakes() {
        // no catalog needed: a gpt model on claude is wrong whatever the
        // catalog says
        let dir = tempfile::TempDir::new().unwrap();
        let mut env = EnvGuard::new();
        env.set("CODEX_HOME", dir.path().join("none"));
        env.set("GROK_HOME", dir.path().join("none"));
        assert!(validate_spawn_model("claude", "gpt-5.5").is_some());
        assert!(validate_spawn_model("claude", "grok-4.6").is_some());
        assert!(validate_spawn_model("codex", "claude-opus-5").is_some());
        assert!(validate_spawn_model("grok", "gpt-5.6-sol").is_some());
        // same family without a catalog still fails open
        assert_eq!(validate_spawn_model("claude", "claude-opus-5"), None);
    }
}
