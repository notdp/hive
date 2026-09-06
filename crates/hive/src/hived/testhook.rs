// --------------------------------------------------------------------------
// test hook: one process-global environment double. Closures instead of
// data so each test wires exactly the behavior it needs and nothing else.
// --------------------------------------------------------------------------

use super::{AdapterHandle, ExecOutcome, HivedServerApi, OutputMonitor};
use crate::adapters::base::GateResult;
use crate::adapters::claude_bg::{EngineSession, PaneJob};
use crate::adapters::claude_sessions::ClaudeSession;
use crate::adapters::claude_view::PaneView;
use crate::adapters::codex_app_server::ThreadRuntime;
use crate::adapters::grok_leader::{SessionRecord, SessionRuntime};
use crate::agent::{Agent, DeliveryError};
use crate::team::Team;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub type F0<R> = Arc<dyn Fn() -> R + Send + Sync>;
pub type S1<R> = Arc<dyn Fn(&str) -> R + Send + Sync>;
pub type S2<R> = Arc<dyn Fn(&str, &str) -> R + Send + Sync>;
pub type S3<R> = Arc<dyn Fn(&str, &str, &str) -> R + Send + Sync>;
pub type S4<R> = Arc<dyn Fn(&str, &str, &str, &str) -> R + Send + Sync>;
pub type A1<R> = Arc<dyn Fn(&Agent) -> R + Send + Sync>;
pub type P1<R> = Arc<dyn Fn(&Path) -> R + Send + Sync>;
pub type V1<R> = Arc<dyn Fn(&[String]) -> R + Send + Sync>;
pub type FindSessionFile = Arc<dyn Fn(&str, Option<&str>) -> Option<PathBuf> + Send + Sync>;
pub type SessionStatus = Arc<dyn Fn(Option<i32>) -> Option<(String, String)> + Send + Sync>;
pub type WriteHivedOwner = Arc<dyn Fn(&str, i64, &str, &str) + Send + Sync>;
pub type Popen = Arc<dyn Fn(&[String], &Path) -> i32 + Send + Sync>;
pub type JobRows = Vec<Map<String, Value>>;

/// The two adapter methods the hived consumes
/// (`resolve_current_session_id` / `find_session_file`).
#[derive(Clone)]
pub struct FakeAdapter {
    pub resolve: S1<Option<String>>,
    pub find: FindSessionFile,
}

#[derive(Default)]
pub struct Hook {
    // adapters / gate
    pub adapters_get: Option<S1<Option<AdapterHandle>>>,
    pub check_input_gate: Option<P1<GateResult>>,
    // tmux
    pub is_pane_alive: Option<S1<bool>>,
    pub display_value: Option<S2<Option<String>>>,
    pub get_most_recent_client_window: Option<S1<Option<String>>>,
    pub get_pane_window_target: Option<S1<Option<String>>>,
    pub get_window_option: Option<S2<Option<String>>>,
    pub set_pane_option: Option<S3<()>>,
    pub set_window_option: Option<S3<()>>,
    pub send_keys: Option<S2<()>>,
    pub list_panes_all: Option<F0<Vec<crate::tmux::PaneInfo>>>,
    pub is_tmux_window_alive: Option<S1<bool>>,
    // agent_cli
    pub detect_cli_process_for_pane: Option<S1<Option<&'static crate::agent_cli::CLIProfile>>>,
    pub detect_profile_for_pane: Option<S1<Option<&'static crate::agent_cli::CLIProfile>>>,
    pub claude_pid_for_pane: Option<S1<Option<i32>>>,
    pub resolve_model_for_pane: Option<S3<String>>,
    pub member_role_for_pane: Option<S1<&'static str>>,
    // claude_bg
    pub cb_read_pane_job: Option<S1<Option<PaneJob>>>,
    pub cb_engine_session_for_job: Option<S1<Option<EngineSession>>>,
    pub cb_list_jobs: Option<F0<Option<JobRows>>>,
    pub cb_job_id_for_pane: Option<S1<Option<String>>>,
    pub cb_list_recorded_panes: Option<F0<Vec<String>>>,
    pub cb_clear_pane_job: Option<S1<()>>,
    pub cb_stop_job: Option<S1<()>>,
    pub ensure_job_named: Option<S2<()>>,
    // claude_sessions
    pub cs_session_status: Option<SessionStatus>,
    pub cs_list_sessions: Option<F0<Vec<ClaudeSession>>>,
    // claude_view
    pub cv_journal_signature: Option<F0<Vec<String>>>,
    pub cv_view_for_pane: Option<S1<PaneView>>,
    // codex_app_server
    pub cas_runtime_for_pane: Option<S1<Option<ThreadRuntime>>>,
    pub cas_runtime_for_thread: Option<S1<Option<ThreadRuntime>>>,
    pub cas_turn_open_for_thread: Option<S1<Option<bool>>>,
    pub cas_session_id_for_pane: Option<S1<Option<String>>>,
    pub cas_shared_socket_path: Option<F0<PathBuf>>,
    pub cas_daemon_alive: Option<F0<bool>>,
    pub cas_thread_id_for_pane: Option<S1<Option<String>>>,
    pub cas_list_recorded_panes: Option<F0<Vec<String>>>,
    pub cas_clear_pane_thread: Option<S1<()>>,
    pub cas_drop_client: Option<F0<()>>,
    pub cas_spawn_daemon: Option<F0<bool>>,
    pub cas_connect: Option<F0<bool>>,
    // grok_leader
    pub gl_runtime_for_pane: Option<S1<Option<SessionRuntime>>>,
    pub gl_runtime_for_key: Option<S1<Option<SessionRuntime>>>,
    pub gl_session_id_for_pane: Option<S1<Option<String>>>,
    pub gl_read_session_key: Option<S1<Option<SessionRecord>>>,
    pub gl_list_daemon_keys: Option<F0<Vec<String>>>,
    pub gl_socket_path_for_key: Option<S1<PathBuf>>,
    pub gl_kill_daemon_key: Option<S1<()>>,
    pub gl_pool_drop_key: Option<S1<()>>,
    pub gl_connect_pane: Option<S1<bool>>,
    // notify / plugin
    #[allow(clippy::type_complexity)]
    pub notify_debug_emit: Option<Arc<dyn Fn(&str, &str, &[(&str, Value)]) + Send + Sync>>,
    pub notify_ui_notify: Option<S3<(bool, Option<String>)>>,
    #[allow(clippy::type_complexity)]
    pub clear_stale_notify: Option<Arc<dyn Fn(&str, &[String], &str, &str, &str) + Send + Sync>>,
    pub is_plugin_enabled: Option<S1<bool>>,
    // team / agent
    pub team_load: Option<S1<anyhow::Result<Team>>>,
    pub agent_is_alive: Option<A1<bool>>,
    #[allow(clippy::type_complexity)]
    pub agent_send:
        Option<Arc<dyn Fn(&Agent, &str, &str) -> Result<String, DeliveryError> + Send + Sync>>,
    // hived self-seams
    pub resolve_live_agent: Option<S2<anyhow::Result<(Team, Agent)>>>,
    pub check_send_gate: Option<A1<anyhow::Result<()>>>,
    pub member_runtime_payload: Option<S2<Map<String, Value>>>,
    pub busy_output_payload: Option<S1<Map<String, Value>>>,
    pub native_daemon_busy: Option<S1<Option<bool>>>,
    #[allow(clippy::type_complexity)]
    pub transcript_progressed_recently:
        Option<Arc<dyn Fn(&str, f64) -> Option<bool> + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    pub resolve_transcript_path_cached:
        Option<Arc<dyn Fn(&str, bool) -> Option<String> + Send + Sync>>,
    pub claude_bg_runtime: Option<S1<Option<Map<String, Value>>>>,
    pub codex_app_server_runtime: Option<S1<Option<Map<String, Value>>>>,
    pub idle_notify_agent_panes: Option<S1<Vec<String>>>,
    #[allow(clippy::type_complexity)]
    pub team_member_bindings: Option<
        Arc<dyn Fn(&str) -> anyhow::Result<Vec<(String, Map<String, Value>)>> + Send + Sync>,
    >,
    pub fresh_snapshot_session_id: Option<S1<String>>,
    // sockets / lifecycle
    pub request_ping: Option<S1<Option<Map<String, Value>>>>,
    pub cleanup_socket: Option<S1<()>>,
    pub run_dir: Option<S1<PathBuf>>,
    pub write_hived_owner: Option<WriteHivedOwner>,
    pub release_reexec_lock_fd: Option<Arc<dyn Fn(Option<i32>) + Send + Sync>>,
    pub try_acquire_reexec_lock: Option<S1<Option<i32>>>,
    pub execv: Option<V1<ExecOutcome>>,
    pub compute_build_hash: Option<F0<String>>,
    pub stale_disk_build_hash: Option<F0<Option<String>>>,
    pub serve_requests: Option<F0<bool>>,
    #[allow(clippy::type_complexity)]
    pub open_server_socket:
        Option<Arc<dyn Fn(&str) -> anyhow::Result<Box<dyn HivedServerApi>> + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    pub handle_request:
        Option<Arc<dyn Fn(&Map<String, Value>) -> (Map<String, Value>, bool) + Send + Sync>>,
    pub current_exe: Option<F0<String>>,
    pub popen: Option<Popen>,
    pub ignore_sigint: Option<F0<()>>,
    pub hived_loop: Option<S4<()>>,
    pub make_busy_monitor: Option<S1<Option<Arc<dyn OutputMonitor>>>>,
}

pub static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
        super::SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
        super::transcript_path_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *super::claude_jobs_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        super::runtime_snapshots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        super::codex_reattach_at()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        super::unread_pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        super::set_output_busy_monitor(None);
    }
}

pub fn install(hook: Hook) -> Guard {
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    Guard
}

/// Mutate the installed hook in place mid-test.
pub fn update(f: impl FnOnce(&mut Hook)) {
    if let Some(hook) = HOOK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        f(hook);
    }
}
