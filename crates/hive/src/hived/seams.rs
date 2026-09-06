// --------------------------------------------------------------------------
// seams. Each hooked_* consults the process-global test hook, then falls
// through to the real module. The hook is process-global (not thread-local)
// because hived work crosses threads (request handlers); nextest's
// process-per-test keeps it isolated.
// --------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::thread;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::adapters::claude_bg::PaneJob;
use crate::adapters::grok_leader::SessionRecord;
use crate::agent::{Agent, DeliveryError};
use crate::team::Team;

use super::*;

#[cfg(test)]
pub(super) fn hookget<T>(f: impl FnOnce(&testhook::Hook) -> T) -> Option<T> {
    testhook::HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(f)
}

/// Adapter dispatch used by the runtime probes.
pub enum AdapterHandle {
    Real(Box<dyn crate::adapters::base::SessionAdapter>),
    #[cfg(test)]
    Fake(testhook::FakeAdapter),
}

impl AdapterHandle {
    pub fn resolve_current_session_id(&self, pane_id: &str) -> Option<String> {
        match self {
            AdapterHandle::Real(adapter) => adapter.resolve_current_session_id(pane_id),
            #[cfg(test)]
            AdapterHandle::Fake(fake) => (fake.resolve)(pane_id),
        }
    }

    pub fn find_session_file(&self, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
        match self {
            AdapterHandle::Real(adapter) => adapter.find_session_file(session_id, cwd),
            #[cfg(test)]
            AdapterHandle::Fake(fake) => (fake.find)(session_id, cwd),
        }
    }
}

pub(super) fn hooked_adapters_get(name: &str) -> Option<AdapterHandle> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.adapters_get.clone()).flatten() {
        return f(name);
    }
    let adapter: Box<dyn crate::adapters::base::SessionAdapter> = match name {
        "claude" => Box::new(crate::adapters::claude::ClaudeAdapter),
        "codex" => Box::new(crate::adapters::codex::CodexAdapter),
        "grok" => Box::new(crate::adapters::grok::GrokAdapter),
        _ => return None,
    };
    Some(AdapterHandle::Real(adapter))
}

// --- tmux seams ------------------------------------------------------------

pub(super) fn hooked_is_pane_alive(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_pane_alive.clone()).flatten() {
        return f(pane_id);
    }
    crate::tmux::is_pane_alive(pane_id)
}

pub(super) fn hooked_display_value(target: &str, fmt: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.display_value.clone()).flatten() {
        return f(target, fmt);
    }
    crate::tmux::display_value(target, fmt)
}

pub(super) fn hooked_get_most_recent_client_window(session_name: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_most_recent_client_window.clone()).flatten() {
        return f(session_name);
    }
    crate::tmux::get_most_recent_client_window(if session_name.is_empty() {
        None
    } else {
        Some(session_name)
    })
}

pub(super) fn hooked_get_pane_window_target(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_pane_window_target.clone()).flatten() {
        return f(pane_id);
    }
    crate::tmux::get_pane_window_target(pane_id)
}

pub(super) fn hooked_get_window_option(target: &str, key: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.get_window_option.clone()).flatten() {
        return f(target, key);
    }
    crate::tmux::get_window_option(target, key)
}

pub(super) fn hooked_set_pane_option(pane_id: &str, key: &str, value: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.set_pane_option.clone()).flatten() {
        f(pane_id, key, value);
        return;
    }
    crate::tmux::set_pane_option(pane_id, key, value)
}

pub(super) fn hooked_set_window_option(target: &str, option: &str, value: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.set_window_option.clone()).flatten() {
        f(target, option, value);
        return;
    }
    crate::tmux::set_window_option(target, option, value)
}

pub(super) fn hooked_send_keys(pane_id: &str, text: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.send_keys.clone()).flatten() {
        f(pane_id, text);
        return;
    }
    let _ = crate::tmux::send_keys(pane_id, text, true);
}

pub(super) fn hooked_list_panes_all() -> Vec<crate::tmux::PaneInfo> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.list_panes_all.clone()).flatten() {
        return f();
    }
    crate::tmux::list_panes_all()
}

pub(super) fn hooked_is_tmux_window_alive(tmux_window_id: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_tmux_window_alive.clone()).flatten() {
        return f(tmux_window_id);
    }
    is_tmux_window_alive_impl(tmux_window_id)
}

// --- agent_cli seams -------------------------------------------------------

pub(super) fn hooked_detect_cli_process_for_pane(
    pane_id: &str,
) -> Option<&'static crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.detect_cli_process_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::detect_cli_process_for_pane(pane_id)
}

pub(super) fn hooked_detect_profile_for_pane(
    pane_id: &str,
) -> Option<&'static crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.detect_profile_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::detect_profile_for_pane(pane_id)
}

pub(super) fn hooked_claude_pid_for_pane(pane_id: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.claude_pid_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::claude_pid_for_pane(pane_id)
}

pub(super) fn hooked_resolve_model_for_pane(
    pane_id: &str,
    cli_name: &str,
    current_model: &str,
) -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_model_for_pane.clone()).flatten() {
        return f(pane_id, cli_name, current_model);
    }
    crate::agent_cli::resolve_model_for_pane(pane_id, cli_name, current_model)
}

pub(super) fn hooked_member_role_for_pane(pane_id: &str) -> &'static str {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.member_role_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::agent_cli::member_role_for_pane(pane_id)
}

pub(super) fn hooked_check_input_gate(path: &Path) -> crate::adapters::base::GateResult {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.check_input_gate.clone()).flatten() {
        return f(path);
    }
    crate::adapters::base::check_input_gate(path)
}

// --- claude_bg seams -------------------------------------------------------

pub(super) fn hooked_cb_read_pane_job(pane: &str) -> Option<PaneJob> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_read_pane_job.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::claude_bg::read_pane_job(pane)
}

pub(super) fn hooked_cb_engine_session_for_job(
    job_id: &str,
) -> Option<crate::adapters::claude_bg::EngineSession> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_engine_session_for_job.clone()).flatten() {
        return f(job_id);
    }
    crate::adapters::claude_bg::engine_session_for_job(job_id)
}

pub(super) fn hooked_cb_list_jobs() -> Option<Vec<Map<String, Value>>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_list_jobs.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_bg::list_jobs("claude")
}

pub(super) fn hooked_cb_job_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_job_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::claude_bg::job_id_for_pane(pane)
}

pub(super) fn hooked_cb_list_recorded_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_list_recorded_panes.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_bg::list_recorded_panes()
}

pub(super) fn hooked_cb_clear_pane_job(pane: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_clear_pane_job.clone()).flatten() {
        f(pane);
        return;
    }
    crate::adapters::claude_bg::clear_pane_job(pane)
}

pub(super) fn hooked_cb_stop_job(job_id: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cb_stop_job.clone()).flatten() {
        f(job_id);
        return;
    }
    crate::adapters::claude_bg::stop_job(job_id, "claude")
}

pub(super) fn hooked_ensure_job_named_thread(job_id: &str, want: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.ensure_job_named.clone()).flatten() {
        f(job_id, want);
        return;
    }
    let job_id = job_id.to_string();
    let want = want.to_string();
    let _ = thread::Builder::new().spawn(move || {
        let _ = crate::adapters::claude_bg::ensure_job_named(&job_id, &want);
    });
}

// --- claude_sessions seams -------------------------------------------------

pub(super) fn hooked_cs_session_status(pid: Option<i32>) -> Option<(String, String)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cs_session_status.clone()).flatten() {
        return f(pid);
    }
    crate::adapters::claude_sessions::session_status(pid)
}

pub(super) fn hooked_cs_list_sessions() -> Vec<crate::adapters::claude_sessions::ClaudeSession> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cs_list_sessions.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_sessions::list_sessions()
}

// --- claude_view seams -----------------------------------------------------

pub(super) fn hooked_cv_journal_signature() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cv_journal_signature.clone()).flatten() {
        return f();
    }
    crate::adapters::claude_view::journal_signature()
}

pub(super) fn hooked_cv_view_for_pane(
    pane_id: &str,
    panes: Option<&[crate::tmux::PaneInfo]>,
) -> crate::adapters::claude_view::PaneView {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cv_view_for_pane.clone()).flatten() {
        return f(pane_id);
    }
    crate::adapters::claude_view::view_for_pane(pane_id, panes)
}

// --- codex_app_server seams ------------------------------------------------

pub(super) fn hooked_cas_runtime_for_pane(
    pane: &str,
) -> Option<crate::adapters::codex_app_server::ThreadRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_runtime_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::runtime_for_pane(pane)
}

pub(super) fn hooked_cas_runtime_for_thread(
    thread_id: &str,
) -> Option<crate::adapters::codex_app_server::ThreadRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_runtime_for_thread.clone()).flatten() {
        return f(thread_id);
    }
    crate::adapters::codex_app_server::runtime_for_thread(thread_id)
}

pub(super) fn hooked_cas_session_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_session_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::session_id_for_pane(pane)
}

pub(super) fn hooked_cas_shared_socket_path() -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_shared_socket_path.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::shared_socket_path()
}

pub(super) fn hooked_cas_daemon_alive() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_daemon_alive.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::daemon_alive()
}

pub(super) fn hooked_cas_thread_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_thread_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::codex_app_server::thread_id_for_pane(pane)
}

pub(super) fn hooked_cas_list_recorded_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_list_recorded_panes.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::list_recorded_panes()
}

pub(super) fn hooked_cas_clear_pane_thread(pane: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_clear_pane_thread.clone()).flatten() {
        f(pane);
        return;
    }
    let _ = crate::adapters::codex_app_server::clear_pane_thread(pane);
}

pub(super) fn hooked_cas_drop_client() {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_drop_client.clone()).flatten() {
        f();
        return;
    }
    crate::adapters::codex_app_server::drop_client()
}

pub(super) fn hooked_cas_spawn_daemon() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_spawn_daemon.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::spawn_daemon()
}

pub(super) fn hooked_cas_connect() -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cas_connect.clone()).flatten() {
        return f();
    }
    crate::adapters::codex_app_server::connect()
}

// --- grok_leader seams -----------------------------------------------------

pub(super) fn hooked_gl_runtime_for_pane(
    pane: &str,
) -> Option<crate::adapters::grok_leader::SessionRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_runtime_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::runtime_for_pane(pane)
}

pub(super) fn hooked_gl_runtime_for_key(
    key: &str,
) -> Option<crate::adapters::grok_leader::SessionRuntime> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_runtime_for_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::runtime_for_key(key)
}

pub(super) fn hooked_gl_session_id_for_pane(pane: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_session_id_for_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::session_id_for_pane(pane)
}

pub(super) fn hooked_gl_read_session_key(key: &str) -> Option<SessionRecord> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_read_session_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::read_session_key(key)
}

pub(super) fn hooked_gl_list_daemon_keys() -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_list_daemon_keys.clone()).flatten() {
        return f();
    }
    crate::adapters::grok_leader::list_daemon_keys()
}

pub(super) fn hooked_gl_socket_path_for_key(key: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_socket_path_for_key.clone()).flatten() {
        return f(key);
    }
    crate::adapters::grok_leader::socket_path_for_key(key)
}

pub(super) fn hooked_gl_kill_daemon_key(key: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_kill_daemon_key.clone()).flatten() {
        f(key);
        return;
    }
    crate::adapters::grok_leader::kill_daemon_key(key)
}

pub(super) fn hooked_gl_pool_drop_key(key: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_pool_drop_key.clone()).flatten() {
        f(key);
        return;
    }
    crate::adapters::grok_leader::pool().drop_key(key)
}

pub(super) fn hooked_gl_connect_pane(pane: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.gl_connect_pane.clone()).flatten() {
        return f(pane);
    }
    crate::adapters::grok_leader::connect_pane(pane)
}

// --- notify / plugin seams -------------------------------------------------

pub(super) fn hooked_notify_debug_emit(workspace: &str, event: &str, fields: &[(&str, Value)]) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.notify_debug_emit.clone()).flatten() {
        f(workspace, event, fields);
        return;
    }
    crate::notify_debug::emit(workspace, event, fields)
}

pub(super) fn hooked_notify_ui_notify(
    message: &str,
    pane_id: &str,
    workspace: &str,
) -> (bool, Option<String>) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.notify_ui_notify.clone()).flatten() {
        return f(message, pane_id, workspace);
    }
    match crate::notify_ui::notify(message, pane_id, workspace) {
        Ok(payload) => (payload.suppressed, Some(payload.surface)),
        Err(_) => (false, None),
    }
}

pub(super) fn hooked_clear_stale_notify(
    window_target: &str,
    panes: &[String],
    token: &str,
    source: &str,
    workspace: &str,
) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.clear_stale_notify.clone()).flatten() {
        f(window_target, panes, token, source, workspace);
        return;
    }
    crate::notify_ui::clear_stale_notify(window_target, panes, token, source, workspace)
}

pub(super) fn hooked_is_plugin_enabled(name: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.is_plugin_enabled.clone()).flatten() {
        return f(name);
    }
    crate::plugin_manager::is_plugin_enabled(name)
}

// --- team / agent seams ----------------------------------------------------

pub(super) fn hooked_team_load(name: &str) -> Result<Team> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.team_load.clone()).flatten() {
        return f(name);
    }
    Team::load(name, "")
}

pub(super) fn hooked_agent_is_alive(agent: &Agent) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.agent_is_alive.clone()).flatten() {
        return f(agent);
    }
    agent.is_alive()
}

pub(super) fn hooked_agent_send(
    agent: &Agent,
    text: &str,
    sender: &str,
) -> std::result::Result<String, DeliveryError> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.agent_send.clone()).flatten() {
        return f(agent, text, sender);
    }
    agent.send_from(text, sender)
}

// --- self seams (this module's own entry points, replaceable in tests) ----

fn resolve_live_agent(team_name: &str, agent_name: &str) -> Result<(Team, Agent)> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_live_agent.clone()).flatten() {
        return f(team_name, agent_name);
    }
    resolve_live_agent_impl(team_name, agent_name)
}

pub(super) fn hooked_resolve_live_agent(
    team_name: &str,
    agent_name: &str,
) -> Result<(Team, Agent)> {
    resolve_live_agent(team_name, agent_name)
}

fn check_send_gate(target: &Agent) -> Result<()> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.check_send_gate.clone()).flatten() {
        return f(target);
    }
    check_send_gate_impl(target)
}

pub(super) fn hooked_check_send_gate(target: &Agent) -> Result<()> {
    check_send_gate(target)
}

fn member_runtime_payload(pane_id: &str, role: &str) -> Map<String, Value> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.member_runtime_payload.clone()).flatten() {
        return f(pane_id, role);
    }
    member_runtime_payload_impl(pane_id, role)
}

pub(super) fn hooked_member_runtime_payload(pane_id: &str, role: &str) -> Map<String, Value> {
    member_runtime_payload(pane_id, role)
}

fn busy_output_payload(pane_id: &str) -> Map<String, Value> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.busy_output_payload.clone()).flatten() {
        return f(pane_id);
    }
    busy_output_payload_impl(pane_id)
}

pub(super) fn hooked_busy_output_payload(pane_id: &str) -> Map<String, Value> {
    busy_output_payload(pane_id)
}

pub(crate) fn native_daemon_busy(pane_id: &str) -> Option<bool> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.native_daemon_busy.clone()).flatten() {
        return f(pane_id);
    }
    native_daemon_busy_impl(pane_id)
}

pub(super) fn hooked_native_daemon_busy(pane_id: &str) -> Option<bool> {
    native_daemon_busy(pane_id)
}

pub(crate) fn transcript_progressed_recently(
    pane_id: &str,
    threshold_seconds: f64,
) -> Option<bool> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.transcript_progressed_recently.clone()).flatten() {
        return f(pane_id, threshold_seconds);
    }
    transcript_progressed_recently_impl(pane_id, threshold_seconds)
}

pub(super) fn hooked_transcript_progressed_recently(
    pane_id: &str,
    threshold_seconds: f64,
) -> Option<bool> {
    transcript_progressed_recently(pane_id, threshold_seconds)
}

pub(crate) fn resolve_transcript_path_cached(pane_id: &str, force: bool) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.resolve_transcript_path_cached.clone()).flatten() {
        return f(pane_id, force);
    }
    resolve_transcript_path_cached_impl(pane_id, force)
}

pub(super) fn hooked_resolve_transcript_path_cached(pane_id: &str, force: bool) -> Option<String> {
    resolve_transcript_path_cached(pane_id, force)
}

pub(crate) fn claude_bg_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.claude_bg_runtime.clone()).flatten() {
        return f(pane_id);
    }
    claude_bg_runtime_impl(pane_id)
}

pub(super) fn hooked_claude_bg_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    claude_bg_runtime(pane_id)
}

pub(crate) fn codex_app_server_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.codex_app_server_runtime.clone()).flatten() {
        return f(pane_id);
    }
    codex_app_server_runtime_impl(pane_id)
}

pub(super) fn hooked_codex_app_server_runtime(pane_id: &str) -> Option<Map<String, Value>> {
    codex_app_server_runtime(pane_id)
}

pub(crate) fn idle_notify_agent_panes(team_name: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.idle_notify_agent_panes.clone()).flatten() {
        return f(team_name);
    }
    idle_notify_agent_panes_impl(team_name)
}

pub(super) fn hooked_idle_notify_agent_panes(team_name: &str) -> Vec<String> {
    idle_notify_agent_panes(team_name)
}

fn team_member_bindings(team_name: &str) -> Result<Vec<(String, Map<String, Value>)>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.team_member_bindings.clone()).flatten() {
        return f(team_name);
    }
    team_member_bindings_impl(team_name)
}

pub(super) fn hooked_team_member_bindings(
    team_name: &str,
) -> Result<Vec<(String, Map<String, Value>)>> {
    team_member_bindings(team_name)
}

fn fresh_snapshot_session_id(pane_id: &str, now: Option<f64>) -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.fresh_snapshot_session_id.clone()).flatten() {
        return f(pane_id);
    }
    fresh_snapshot_session_id_impl(pane_id, now)
}

pub(super) fn hooked_fresh_snapshot_session_id(pane_id: &str, now: Option<f64>) -> String {
    fresh_snapshot_session_id(pane_id, now)
}

pub fn request_ping(workspace: &str) -> Option<Map<String, Value>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.request_ping.clone()).flatten() {
        return f(workspace);
    }
    request_ping_impl(workspace)
}

pub(super) fn hooked_request_ping(workspace: &str) -> Option<Map<String, Value>> {
    request_ping(workspace)
}

fn cleanup_socket(workspace: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.cleanup_socket.clone()).flatten() {
        f(workspace);
        return;
    }
    cleanup_socket_impl(workspace)
}

pub(super) fn hooked_cleanup_socket(workspace: &str) {
    cleanup_socket(workspace)
}

pub(super) fn hooked_run_dir(workspace: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.run_dir.clone()).flatten() {
        return f(workspace);
    }
    run_dir_impl(workspace)
}

fn write_hived_owner(workspace: &str, pid: i64, started_at: &str, token: &str) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.write_hived_owner.clone()).flatten() {
        f(workspace, pid, started_at, token);
        return;
    }
    write_hived_owner_impl(workspace, pid, started_at, token)
}

pub(super) fn hooked_write_hived_owner(workspace: &str, pid: i64, started_at: &str, token: &str) {
    write_hived_owner(workspace, pid, started_at, token)
}

pub(crate) fn release_reexec_lock_fd(lock_fd: Option<i32>) {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.release_reexec_lock_fd.clone()).flatten() {
        f(lock_fd);
        return;
    }
    release_reexec_lock_fd_impl(lock_fd)
}

pub(super) fn hooked_release_reexec_lock_fd(lock_fd: Option<i32>) {
    release_reexec_lock_fd(lock_fd)
}

pub(crate) fn try_acquire_reexec_lock(workspace: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.try_acquire_reexec_lock.clone()).flatten() {
        return f(workspace);
    }
    try_acquire_reexec_lock_impl(workspace)
}

pub(super) fn hooked_try_acquire_reexec_lock(workspace: &str) -> Option<i32> {
    try_acquire_reexec_lock(workspace)
}

pub(super) fn hooked_execv(argv: &[String]) -> ExecOutcome {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.execv.clone()).flatten() {
        return f(argv);
    }
    execv_impl(argv)
}

pub(super) fn hooked_compute_build_hash() -> String {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.compute_build_hash.clone()).flatten() {
        return f();
    }
    compute_build_hash()
}

pub(super) fn hooked_stale_disk_build_hash(state: &mut ReexecState, now: f64) -> Option<String> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.stale_disk_build_hash.clone()).flatten() {
        return f();
    }
    stale_disk_build_hash_for_reexec(state, now)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn hooked_serve_requests(
    server: &dyn HivedServerApi,
    workspace: &str,
    team: &str,
    tmux_window: &str,
    tmux_window_id: &str,
    hived_started_at: &str,
    timeout: f64,
) -> bool {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.serve_requests.clone()).flatten() {
        return f();
    }
    serve_requests(
        server,
        workspace,
        team,
        tmux_window,
        tmux_window_id,
        hived_started_at,
        timeout,
    )
}

pub(super) fn hooked_open_server_socket(workspace: &str) -> Result<Box<dyn HivedServerApi>> {
    #[cfg(test)]
    if let Some(f) = hookget(|h| h.open_server_socket.clone()).flatten() {
        return f(workspace);
    }
    Ok(Box::new(open_server_socket(workspace)?))
}
