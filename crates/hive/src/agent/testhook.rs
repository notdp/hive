// ---------------------------------------------------------------------------
// Test hook: one thread-local environment double, mirroring what the Python
// suite pins with monkeypatch (`_setup_tmux_mocks` defaults in `Hook::new`).
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::collections::HashMap;

use crate::adapters::claude_bg::{EngineSession, KeyResult};
use crate::adapters::claude_sessions::ClaudeSession;
use crate::agent::Agent;

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRecord {
    pub cwd: String,
    pub name: String,
    pub prompt: String,
    pub extra_args: Vec<String>,
    pub extra_env: HashMap<String, String>,
}

/// A member row with the fields every unit suite leaves blank; callers
/// override the rest with struct update syntax.
pub fn fake_agent(name: &str, team: &str, pane: &str, cli: &str) -> Agent {
    Agent {
        name: name.to_string(),
        team_name: team.to_string(),
        pane_id: pane.to_string(),
        model: String::new(),
        cwd: "/repo".to_string(),
        session_id: None,
        cli: cli.to_string(),
    }
}

/// A bg engine registry entry as engine_session_for_job would return it.
pub fn fake_engine(pid: i32, job_id: &str, session_id: &str) -> EngineSession {
    EngineSession {
        pid,
        job_id: job_id.to_string(),
        session_id: session_id.to_string(),
        socket_path: format!("/tmp/hive-test-inbox-{pid}.sock"),
        cwd: "/tmp".to_string(),
        status: "idle".to_string(),
        waiting_for: String::new(),
        status_updated_at: 0.0,
        name: String::new(),
    }
}

#[allow(dead_code)]
#[derive(Default)]
pub struct Hook {
    // records
    pub calls: Vec<String>,
    pub tags: Vec<(String, String, String, String)>,
    pub titles: Vec<(String, String)>,
    pub killed: Vec<String>,
    pub cleared_tags: Vec<String>,
    pub captured: Vec<(String, u32)>,
    pub sleeps: Vec<f64>,
    pub cancelled_modes: Vec<String>,
    pub buffers_loaded: Vec<(String, String)>,
    pub pasted: Vec<(String, String)>,
    pub deleted_buffers: Vec<String>,
    pub draft_cleared: Vec<String>,
    pub resolved_session_panes: Vec<String>,
    pub event_order: Vec<String>,

    pub spawns: Vec<SpawnRecord>,
    pub wakes: Vec<String>,
    pub records: Vec<(String, String, String, String)>,
    pub stopped: Vec<String>,
    pub pane_job_lookups: Vec<String>,
    pub seen_jobs: Vec<String>,
    pub typed: Vec<(String, String)>,
    pub interrupted_jobs: Vec<String>,

    pub codex_started: Vec<()>,
    pub codex_minted: Vec<(String, String, String)>,
    pub codex_trusted: Vec<String>,
    pub codex_records: Vec<(String, String, String)>,
    pub codex_sent: Vec<(String, String)>,
    pub codex_sent_thread: Vec<(String, String)>,
    pub codex_interrupted_panes: Vec<String>,
    pub codex_interrupted_threads: Vec<String>,

    pub grok_started: Vec<String>,
    pub grok_sessions: Vec<(String, String, String)>,
    pub grok_sent: Vec<(String, String)>,
    pub grok_sent_key: Vec<(String, String)>,
    pub grok_interrupted_panes: Vec<String>,
    pub grok_interrupted_keys: Vec<String>,
    pub grok_killed_keys: Vec<String>,
    pub waited_pane_gone: Vec<String>,

    pub inbox_writes: Vec<(String, String, String, String)>,
    pub daemon_replies: Vec<(String, String)>,

    pub connects_codex: Vec<String>,
    pub connects_grok: Vec<(String, String)>,
    pub waited_codex: Vec<String>,
    pub waited_grok: Vec<(String, String)>,

    // behaviors (Hook::new sets the `_setup_tmux_mocks` defaults)
    pub is_inside_tmux: bool,
    /// None → echo the target pane; Some(Err(msg)) → the split fails with msg.
    pub split_window_result: Option<Result<String, String>>,
    pub pane_window_target: String,
    pub is_pane_in_mode: bool,
    pub supported_profile: bool,
    pub parse_draft: Option<String>,
    pub load_buffer_fails: bool,
    pub clear_input_fails: bool,
    pub resolve_profile_name: Option<String>,
    pub interactive_claude_pid: Option<i32>,
    pub cli_probe: Option<String>, // "" or unset → no live CLI on the pane
    pub cli_probe_seq: Vec<Option<String>>, // consumed first when non-empty
    pub session_ids_by_pane: HashMap<String, String>,
    pub wait_codex_attached: Option<bool>, // None → run the real wait
    pub wait_grok_ready: Option<bool>,     // None → run the real wait

    pub spawn_job_result: Option<String>,
    pub wait_engine_entry: Option<EngineSession>,
    pub ensure_engine: Option<Option<EngineSession>>, // None → echo-jid engine
    pub job_id_for_pane: Option<String>,
    pub job_row_ids: Vec<String>, // job_row answers Some({"id": jid}) for these
    pub engines_by_job: HashMap<String, EngineSession>,
    pub type_into_job_result: Option<KeyResult>,
    pub interrupt_job_result: Option<KeyResult>,

    pub daemon_reply: Option<&'static str>,
    pub sessions_send: Option<&'static str>,
    pub list_sessions: Vec<ClaudeSession>,

    pub codex_spawn_daemon: bool,
    /// Some(msg) → ensure_dir_trusted fails with msg (the cwd is still recorded).
    pub ensure_dir_trusted_error: Option<String>,
    pub start_member_thread: Option<String>,
    pub codex_send_to_pane: Option<&'static str>,
    pub codex_send_to_thread: Option<&'static str>,
    pub codex_interrupt_pane: Option<&'static str>,
    pub codex_interrupt_thread: Option<&'static str>,
    pub codex_daemon_alive: Option<bool>,

    pub grok_spawn_daemon: bool,
    pub grok_send_to_pane: Option<&'static str>,
    pub grok_send_to_key: Option<&'static str>,
    pub grok_interrupt_pane: Option<&'static str>,
    pub grok_interrupt_key: Option<&'static str>,
    pub grok_probe_socket: Option<bool>,
    /// `#{pane_pid}` a kill reads before tearing the pane down.
    pub pane_pid: Option<u32>,
}

impl Hook {
    /// Python `_setup_tmux_mocks` equivalents: inside tmux, split echoes
    /// the target, no daemons, readiness waits answer immediately, the
    /// claude bg spawn path succeeds without touching a real binary.
    pub fn new() -> Hook {
        Hook {
            is_inside_tmux: true,
            wait_codex_attached: Some(true),
            wait_grok_ready: Some(true),
            spawn_job_result: Some("abcd1234".to_string()),
            wait_engine_entry: Some(fake_engine(4321, "abcd1234", "sess-registry")),
            start_member_thread: Some("tid-minted".to_string()),
            ..Default::default()
        }
    }
}

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub fn with<T>(f: impl FnOnce(&mut Hook) -> T) -> Option<T> {
    HOOK.with(|cell| cell.borrow_mut().as_mut().map(f))
}

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        HOOK.with(|cell| *cell.borrow_mut() = None);
    }
}

pub fn install(hook: Hook) -> Guard {
    HOOK.with(|cell| *cell.borrow_mut() = Some(hook));
    Guard
}
