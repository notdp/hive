//! Notify UI: the attention marks on the notified window and pane, the
//! terminal bell, and the select-window hook that clears the marks. Hive
//! draws nothing itself — the team session's status bar renders the marks
//! (`tmux/status.rs`), and the pane border shows `[!]`.

use std::fs;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

#[cfg(test)]
use self::tests::fake_tmux as tmux;
use crate::cli::util::shlex_quote;
use crate::notify_debug;
#[cfg(not(test))]
use crate::tmux;

pub const NOTIFY_TOKEN_OPTION: &str = "@hive-notify-token";
pub const HOOK_NAME_OPTION: &str = "@hive-notify-hook";
/// The pending notify text the status bar's second line shows.
pub const NOTIFY_TEXT_OPTION: &str = "@hive-notify-text";
pub const NOTIFY_TEXT_KEY: &str = "hive-notify-text";
pub const PANE_NOTIFY_ACTIVE_KEY: &str = "hive-notify-active";
// Use one stable high-index hook so each notify refreshes the same fast-path
// instead of installing per-notify hook/script pairs that can go stale.
pub const SELECT_HOOK_NAME: &str = "after-select-window[900001]";

/// What `notify` reports back; `hive notify` prints it as JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotifyPayload {
    pub agent: String,
    #[serde(rename = "paneId")]
    pub pane_id: String,
    pub window: String,
    pub tab: String,
    pub message: String,
    #[serde(rename = "clientMode")]
    pub client_mode: String,
    pub surface: String,
    pub suppressed: bool,
    #[serde(rename = "suppressionReason", skip_serializing_if = "Option::is_none")]
    pub suppression_reason: Option<String>,
}

fn or_null(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        Value::String(value.to_string())
    }
}

/// `agent: message` (the message alone without an agent), `#` doubled: the
/// status line draws the option verbatim, where `#[` opens a style.
pub fn attention_text(agent: &str, message: &str) -> String {
    let text = if agent.is_empty() {
        message.to_string()
    } else {
        format!("{agent}: {message}")
    };
    text.replace('#', "##")
}

fn _target_window_is_focused(session_name: &str, window_target: &str) -> bool {
    if session_name.is_empty() || window_target.is_empty() {
        return false;
    }
    match tmux::get_most_recent_client_window(Some(session_name)) {
        Some(active) => !active.is_empty() && active == window_target,
        None => false,
    }
}

fn _select_hook_command() -> String {
    // run-shell executes with the tmux server's environment, not this
    // process's — the hook must name this binary by absolute path.
    let cleanup_cmd = format!(
        "{} notify-hook \
         --cleanup-selected '#{{session_name}}:#{{window_index}}' \
         --client '#{{client_tty}}'",
        shlex_quote(&crate::cli::util::self_exe())
    );
    // This string is parsed by tmux's hook command parser, then by run-shell.
    // Keep the attached-client e2e test in sync if this quoting changes.
    let escaped = cleanup_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let run_cmd = format!("run-shell -b \"{}\"", escaped);
    format!(
        "if-shell -F '#{{?@{},1,0}}' {}",
        NOTIFY_TOKEN_OPTION.trim_start_matches('@'),
        shlex_quote(&run_cmd)
    )
}

pub fn ensure_notify_select_hook(session: &str) {
    if session.is_empty() {
        return;
    }
    let hook_command = _select_hook_command();
    let _ = tmux::_run(
        &[
            "set-hook",
            "-t",
            session,
            SELECT_HOOK_NAME,
            hook_command.as_str(),
        ],
        false,
        5,
    );
}

/// Recover a window from durable notify state.
pub fn clear_stale_notify(
    window_target: &str,
    panes: &[String],
    token: &str,
    source: &str,
    workspace: &str,
) {
    let token = if token.is_empty() {
        tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
            .unwrap_or_default()
    } else {
        token.to_string()
    };

    notify_debug::emit_for_window(
        window_target,
        "clear.start",
        workspace,
        &[
            ("source", json!(source)),
            ("window", json!(window_target)),
            ("token", or_null(&token)),
            ("panes_count", json!(panes.len())),
        ],
    );

    let mut pane_active_matches = 0;
    if !token.is_empty() {
        // Known boundary: only panes still in this window are reconciled here;
        // a pane parked outside it is cleared when it joins back.
        for pane_id in panes {
            if tmux::get_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY).as_deref()
                == Some(token.as_str())
            {
                tmux::clear_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY);
                pane_active_matches += 1;
            }
        }
    }
    // Each clear is its own tmux call: the token goes last, so whoever
    // polls it (the hived, a test) sees every other carrier gone with it.
    tmux::clear_window_option(window_target, NOTIFY_TEXT_OPTION);
    tmux::clear_window_option(window_target, HOOK_NAME_OPTION);
    tmux::clear_window_option(window_target, NOTIFY_TOKEN_OPTION);

    notify_debug::emit_for_window(
        window_target,
        "clear.done",
        workspace,
        &[
            ("source", json!(source)),
            ("window", json!(window_target)),
            ("token", or_null(&token)),
            ("pane_active_matches", json!(pane_active_matches)),
        ],
    );
}

pub fn cleanup_selected_window(window_target: &str, client: &str) -> bool {
    if window_target.is_empty() || window_target.contains("#{") {
        return false;
    }
    let token = tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
        .unwrap_or_default();
    notify_debug::emit_for_window(
        window_target,
        "cleanup_selected.start",
        "",
        &[
            ("window", json!(window_target)),
            ("client", or_null(client)),
            ("token", or_null(&token)),
        ],
    );
    if token.is_empty() {
        return false;
    }
    let panes = tmux::list_panes(window_target);
    clear_stale_notify(window_target, &panes, &token, "select_hook", "");
    true
}

fn _ring_terminal_bell(pane_id: &str, window_target: &str, workspace: &str) {
    let tty_path = tmux::get_pane_tty(pane_id).unwrap_or_default();
    if tty_path.is_empty() {
        notify_debug::emit_for_window(
            window_target,
            "bell",
            workspace,
            &[
                ("pane", json!(pane_id)),
                ("tty_present", json!(false)),
                ("success", json!(false)),
            ],
        );
        return;
    }
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tty_path)
        .and_then(|mut handle| {
            handle.write_all(b"\x07")?;
            handle.flush()
        });
    if written.is_err() {
        notify_debug::emit_for_window(
            window_target,
            "bell",
            workspace,
            &[
                ("pane", json!(pane_id)),
                ("tty_present", json!(true)),
                ("success", json!(false)),
            ],
        );
        return;
    }
    notify_debug::emit_for_window(
        window_target,
        "bell",
        workspace,
        &[
            ("pane", json!(pane_id)),
            ("tty_present", json!(true)),
            ("success", json!(true)),
        ],
    );
}

/// Mark the window and pane for attention: the token and hook name the select
/// hook clears on, the notify text for the status bar's second line, and
/// `@hive-notify-active` on the pane for its chip and border.
pub fn mark_attention(
    message: &str,
    pane_id: &str,
    window_target: &str,
    agent_name: &str,
    workspace: &str,
) -> anyhow::Result<()> {
    let session = window_target
        .rsplit_once(':')
        .map(|(head, _)| head)
        .unwrap_or("");

    ensure_notify_select_hook(session);

    let hook_idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        % 1_000_000_000;
    let token = format!("{}:{}", pane_id, hook_idx);
    let old_token =
        tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
            .unwrap_or_default();
    notify_debug::emit_for_window(
        window_target,
        "attention.start",
        workspace,
        &[
            ("window", json!(window_target)),
            ("pane", json!(pane_id)),
            ("old_token", or_null(&old_token)),
            ("new_token", json!(token)),
        ],
    );

    tmux::set_window_option(window_target, NOTIFY_TOKEN_OPTION, &token);
    tmux::set_window_option(window_target, HOOK_NAME_OPTION, SELECT_HOOK_NAME);
    tmux::set_window_option(
        window_target,
        NOTIFY_TEXT_OPTION,
        &attention_text(agent_name, message),
    );
    tmux::set_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY, &token);

    notify_debug::emit_for_window(
        window_target,
        "attention.done",
        workspace,
        &[
            ("window", json!(window_target)),
            ("pane", json!(pane_id)),
            ("new_token", json!(token)),
        ],
    );
    Ok(())
}

pub fn notify(message: &str, pane_id: &str, workspace: &str) -> anyhow::Result<NotifyPayload> {
    let mut window_target = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let mut session_name = tmux::get_pane_session_name(pane_id).unwrap_or_default();
    // A parked mirror pane sits in a hidden window nobody looks at: the
    // marks go to the team window, whose bar shows the text and whose
    // select hook can clear them.
    if !window_target.is_empty()
        && tmux::get_window_option(&window_target, crate::tmux::HIDDEN_WINDOW_KEY).is_some()
    {
        let team = tmux::get_pane_option(pane_id, "hive-team").unwrap_or_default();
        window_target = tmux::team_window_target(&team).unwrap_or_default();
        session_name = window_target
            .rsplit_once(':')
            .map(|(session, _)| session.to_string())
            .unwrap_or_default();
    }
    let window_name = tmux::get_pane_window_name(pane_id)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "target".to_string());
    let agent_name = tmux::get_pane_option(pane_id, "hive-agent").unwrap_or_default();
    let client_mode = tmux::get_client_mode(Some(pane_id));
    let suppressed = _target_window_is_focused(&session_name, &window_target);
    notify_debug::emit_for_window(
        &window_target,
        "notify.call",
        workspace,
        &[
            ("pane", json!(pane_id)),
            ("window", or_null(&window_target)),
            ("agent", or_null(&agent_name)),
            ("client_mode", json!(client_mode)),
            ("suppressed", json!(suppressed)),
        ],
    );
    if suppressed {
        return Ok(NotifyPayload {
            agent: agent_name,
            pane_id: pane_id.to_string(),
            window: window_target,
            tab: window_name,
            message: message.to_string(),
            client_mode,
            surface: "suppressed".to_string(),
            suppressed: true,
            suppression_reason: Some("focused_window".to_string()),
        });
    }

    if !window_target.is_empty() {
        mark_attention(message, pane_id, &window_target, &agent_name, workspace)?;
    }
    _ring_terminal_bell(pane_id, &window_target, workspace);
    Ok(NotifyPayload {
        agent: agent_name,
        pane_id: pane_id.to_string(),
        window: window_target,
        tab: window_name,
        message: message.to_string(),
        client_mode,
        surface: "fired".to_string(),
        suppressed: false,
        suppression_reason: None,
    })
}

/// The `hive notify-hook` entry point: the after-select-window tmux hook
/// runs it with `--cleanup-selected <window> --client <tty>`.
pub fn main(argv: &[String]) -> i32 {
    let mut cleanup_selected = String::new();
    let mut client = String::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        if let Some(value) = arg.strip_prefix("--cleanup-selected=") {
            cleanup_selected = value.to_string();
        } else if arg == "--cleanup-selected" {
            index += 1;
            cleanup_selected = argv.get(index).cloned().unwrap_or_default();
        } else if let Some(value) = arg.strip_prefix("--client=") {
            client = value.to_string();
        } else if arg == "--client" {
            index += 1;
            client = argv.get(index).cloned().unwrap_or_default();
        }
        index += 1;
    }
    if !cleanup_selected.is_empty() {
        cleanup_selected_window(&cleanup_selected, &client);
        return 0;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Test stand-in for `crate::tmux`.
    pub mod fake_tmux {
        use std::cell::RefCell;
        use std::collections::HashMap;

        #[derive(Default)]
        pub struct FakeState {
            pub window_options: HashMap<String, String>,
            pub pane_options: HashMap<(String, String), String>,
            /// (kind, target-or-pane, option-or-name, value)
            pub actions: Vec<(String, String, String, String)>,
            pub run_calls: Vec<Vec<String>>,
            pub pane_window_name: Option<String>,
            pub pane_window_target: Option<String>,
            pub pane_session_name: Option<String>,
            pub pane_agent: Option<String>,
            pub client_mode: Option<String>,
            pub most_recent_client_window: Option<String>,
            pub pane_tty: Option<String>,
            pub panes: Vec<String>,
            /// Windows answering `@hive-hidden`.
            pub hidden_windows: Vec<String>,
            pub team_window: Option<String>,
        }

        thread_local! {
            static STATE: RefCell<FakeState> = RefCell::new(FakeState::default());
        }

        pub fn reset() {
            STATE.with(|state| *state.borrow_mut() = FakeState::default());
        }

        pub fn with_state<R>(f: impl FnOnce(&mut FakeState) -> R) -> R {
            STATE.with(|state| f(&mut state.borrow_mut()))
        }

        fn strip(option: &str) -> String {
            option.trim_start_matches('@').to_string()
        }

        pub fn get_most_recent_client_window(_session: Option<&str>) -> Option<String> {
            with_state(|state| state.most_recent_client_window.clone())
        }

        pub fn get_client_mode(_target: Option<&str>) -> String {
            with_state(|state| state.client_mode.clone()).unwrap_or_else(|| "unknown".to_string())
        }

        pub fn get_window_option(target: &str, key: &str) -> Option<String> {
            with_state(|state| {
                if key == crate::tmux::HIDDEN_WINDOW_KEY {
                    return state
                        .hidden_windows
                        .iter()
                        .any(|w| w == target)
                        .then(|| "honey".to_string());
                }
                state.window_options.get(key).cloned()
            })
        }

        pub fn team_window_target(_team: &str) -> Option<String> {
            with_state(|state| state.team_window.clone())
        }

        pub fn set_window_option(target: &str, option: &str, value: &str) {
            with_state(|state| {
                state.actions.push((
                    "set-window".to_string(),
                    target.to_string(),
                    option.to_string(),
                    value.to_string(),
                ));
                state
                    .window_options
                    .insert(strip(option), value.to_string());
            });
        }

        pub fn clear_window_option(target: &str, option: &str) {
            with_state(|state| {
                state.actions.push((
                    "clear-window".to_string(),
                    target.to_string(),
                    option.to_string(),
                    String::new(),
                ));
                state.window_options.remove(&strip(option));
            });
        }

        pub fn list_panes(_target: &str) -> Vec<String> {
            with_state(|state| state.panes.clone())
        }

        pub fn get_pane_option(pane: &str, key: &str) -> Option<String> {
            with_state(|state| {
                if key == "hive-agent" {
                    if let Some(agent) = &state.pane_agent {
                        return Some(agent.clone());
                    }
                }
                state
                    .pane_options
                    .get(&(pane.to_string(), key.to_string()))
                    .cloned()
            })
        }

        pub fn set_pane_option(pane: &str, key: &str, value: &str) {
            with_state(|state| {
                state.actions.push((
                    "set-pane".to_string(),
                    pane.to_string(),
                    key.to_string(),
                    value.to_string(),
                ));
                state
                    .pane_options
                    .insert((pane.to_string(), key.to_string()), value.to_string());
            });
        }

        pub fn clear_pane_option(pane: &str, key: &str) {
            with_state(|state| {
                state.actions.push((
                    "clear-pane".to_string(),
                    pane.to_string(),
                    key.to_string(),
                    String::new(),
                ));
                state
                    .pane_options
                    .remove(&(pane.to_string(), key.to_string()));
            });
        }

        pub fn get_pane_tty(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_tty.clone())
        }

        pub fn get_pane_window_target(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_window_target.clone())
        }

        pub fn get_pane_window_name(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_window_name.clone())
        }

        pub fn get_pane_session_name(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_session_name.clone())
        }

        pub fn _run(
            args: &[&str],
            _check: bool,
            _timeout: u64,
        ) -> Result<crate::tmux::Run, crate::tmux::TmuxError> {
            with_state(|state| {
                state
                    .run_calls
                    .push(args.iter().map(|arg| arg.to_string()).collect())
            });
            Ok(crate::tmux::Run {
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn mock_tmux_basics() {
        fake_tmux::reset();
        fake_tmux::with_state(|state| {
            state.pane_window_name = Some("dev".to_string());
            state.pane_window_target = Some("dev:1".to_string());
            state.pane_agent = Some("orch".to_string());
            state.pane_session_name = Some("dev".to_string());
            state.most_recent_client_window = Some("dev:9".to_string());
            state.client_mode = Some("terminal".to_string());
        });
    }

    /// Route notify_debug workspace resolution to a temp dir so no test
    /// writes under the real cache dir.
    fn route_debug_logs(workspace: &std::path::Path) {
        crate::notify_debug::tests::fake_tmux::reset();
        crate::notify_debug::tests::fake_tmux::set_workspace_value(Some(
            workspace.to_string_lossy().into_owned(),
        ));
    }

    fn actions3() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .map(|(kind, a, b, _)| (kind.clone(), a.clone(), b.clone()))
                .collect()
        })
    }

    fn action_kinds() -> Vec<String> {
        fake_tmux::with_state(|state| state.actions.iter().map(|a| a.0.clone()).collect())
    }

    fn set_window_calls() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .filter(|(kind, _, _, _)| kind == "set-window")
                .map(|(_, target, option, value)| (target.clone(), option.clone(), value.clone()))
                .collect()
        })
    }

    fn pane_set_calls() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .filter(|(kind, _, _, _)| kind == "set-pane")
                .map(|(_, pane, key, value)| (pane.clone(), key.clone(), value.clone()))
                .collect()
        })
    }

    fn run_calls() -> Vec<Vec<String>> {
        fake_tmux::with_state(|state| state.run_calls.clone())
    }

    fn set_window_value(option: &str) -> String {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .find(|(kind, _, opt, _)| kind == "set-window" && opt == option)
                .map(|(_, _, _, value)| value.clone())
        })
        .unwrap_or_else(|| panic!("expected a set for {}", option))
    }

    fn owned3(items: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        items
            .iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn test_notify_marks_the_pane_and_window_and_rings_the_bell() {
        let tmp = TempDir::new().unwrap();
        mock_tmux_basics();
        route_debug_logs(&tmp.path().join("ws"));
        let tty = tmp.path().join("tty");
        fs::write(&tty, "").unwrap();
        fake_tmux::with_state(|state| state.pane_tty = Some(tty.to_string_lossy().into_owned()));

        let payload = notify("回来确认", "%9", "").unwrap();

        assert_eq!(payload.surface, "fired");
        assert!(!payload.suppressed);
        assert_eq!(payload.message, "回来确认");
        assert_eq!(payload.pane_id, "%9");
        assert_eq!(payload.window, "dev:1");
        assert_eq!(payload.tab, "dev");
        assert_eq!(payload.agent, "orch");
        let token = set_window_value(NOTIFY_TOKEN_OPTION);
        assert!(token.starts_with("%9:"));
        assert_eq!(
            set_window_calls(),
            owned3(&[
                ("dev:1", "@hive-notify-token", token.as_str()),
                ("dev:1", "@hive-notify-hook", SELECT_HOOK_NAME),
                ("dev:1", "@hive-notify-text", "orch: 回来确认"),
            ])
        );
        assert_eq!(
            pane_set_calls(),
            vec![(
                "%9".to_string(),
                "hive-notify-active".to_string(),
                token.clone()
            )]
        );
        assert!(action_kinds()
            .iter()
            .all(|kind| kind == "set-window" || kind == "set-pane"));
        // bell hit the pane tty
        assert_eq!(fs::read(&tty).unwrap(), b"\x07");
    }

    #[test]
    fn test_notify_from_a_parked_mirror_pane_marks_the_team_window() {
        let tmp = TempDir::new().unwrap();
        mock_tmux_basics();
        route_debug_logs(tmp.path());
        fake_tmux::with_state(|state| {
            state.pane_window_target = Some("honey:9".to_string());
            state.pane_session_name = Some("honey".to_string());
            state.hidden_windows = vec!["honey:9".to_string()];
            state.team_window = Some("dev:1".to_string());
            state.pane_options.insert(
                ("%9".to_string(), "hive-team".to_string()),
                "honey".to_string(),
            );
        });

        let payload = notify("回来确认", "%9", "").unwrap();

        assert_eq!(payload.window, "dev:1");
        assert!(!payload.suppressed);
        assert!(set_window_calls()
            .iter()
            .all(|(target, _, _)| target == "dev:1"));
        assert_eq!(set_window_calls().len(), 3);
        assert_eq!(pane_set_calls().len(), 1);
        assert_eq!(pane_set_calls()[0].0, "%9");
        // The hook is installed on the team window's session.
        assert_eq!(run_calls()[0][..3], v3("set-hook", "-t", "dev"));
    }

    fn v3(a: &str, b: &str, c: &str) -> Vec<String> {
        vec![a.to_string(), b.to_string(), c.to_string()]
    }

    #[test]
    fn test_notify_is_silent_when_target_window_is_focused() {
        let tmp = TempDir::new().unwrap();
        mock_tmux_basics();
        route_debug_logs(tmp.path());
        fake_tmux::with_state(|state| state.most_recent_client_window = Some("dev:1".to_string()));

        let payload = notify("回来确认", "%9", "").unwrap();

        assert_eq!(payload.surface, "suppressed");
        assert!(payload.suppressed);
        assert_eq!(
            payload.suppression_reason.as_deref(),
            Some("focused_window")
        );
        assert!(actions3().is_empty());
        assert!(run_calls().is_empty());
    }

    #[test]
    fn test_attention_text_escapes_hashes_and_drops_an_empty_agent() {
        assert_eq!(attention_text("orch", "a #1"), "orch: a ##1");
        assert_eq!(attention_text("", "m"), "m");
        assert_eq!(attention_text("", "#[x]"), "##[x]");
    }

    #[test]
    fn test_mark_attention_installs_the_hook_once_per_call() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        mark_attention("Agent finished", "%9", "dev:1", "", "").unwrap();

        assert_eq!(set_window_value(NOTIFY_TEXT_OPTION), "Agent finished");
        let runs = run_calls();
        assert_eq!(runs.len(), 1);
        let hook_cmd = &runs[0];
        assert_eq!(
            hook_cmd[..4].to_vec(),
            vec![
                "set-hook".to_string(),
                "-t".to_string(),
                "dev".to_string(),
                SELECT_HOOK_NAME.to_string()
            ]
        );
        assert!(!hook_cmd[4].contains("set-hook -ut"));
        assert!(hook_cmd[4].contains("notify-hook --cleanup-selected"));
        assert!(hook_cmd[4].contains("'#{client_tty}'"));
    }

    #[test]
    fn test_second_mark_attention_replaces_the_text_and_token() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        mark_attention("m1", "%9", "dev:1", "orch", "").unwrap();
        mark_attention("m2", "%9", "dev:1", "orch", "").unwrap();

        fake_tmux::with_state(|state| {
            assert_eq!(
                state
                    .window_options
                    .get(NOTIFY_TEXT_KEY)
                    .map(String::as_str),
                Some("orch: m2")
            );
            let token = state.window_options.get("hive-notify-token").unwrap();
            assert_eq!(
                state
                    .pane_options
                    .get(&("%9".to_string(), PANE_NOTIFY_ACTIVE_KEY.to_string())),
                Some(token)
            );
        });
        assert_eq!(run_calls().len(), 2);
    }

    #[test]
    fn test_clear_stale_notify_clears_the_carriers_and_matching_pane() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());
        fake_tmux::with_state(|state| {
            state
                .window_options
                .insert("hive-notify-token".to_string(), "%9:old-fire".to_string());
            state
                .window_options
                .insert("hive-notify-hook".to_string(), SELECT_HOOK_NAME.to_string());
            state
                .window_options
                .insert(NOTIFY_TEXT_KEY.to_string(), "orch: m1".to_string());
            state.pane_options.insert(
                ("%9".to_string(), "hive-notify-active".to_string()),
                "%9:old-fire".to_string(),
            );
            state.pane_options.insert(
                ("%10".to_string(), "hive-notify-active".to_string()),
                "%10:new-fire".to_string(),
            );
        });

        clear_stale_notify(
            "dev:1",
            &["%9".to_string(), "%10".to_string()],
            "",
            "unknown",
            "",
        );

        // The token, the carrier every poller keys on, goes last.
        assert_eq!(
            actions3(),
            owned3(&[
                ("clear-pane", "%9", "hive-notify-active"),
                ("clear-window", "dev:1", "@hive-notify-text"),
                ("clear-window", "dev:1", "@hive-notify-hook"),
                ("clear-window", "dev:1", "@hive-notify-token"),
            ])
        );
        fake_tmux::with_state(|state| {
            assert!(state.window_options.is_empty());
            assert_eq!(state.pane_options.len(), 1);
            assert_eq!(
                state
                    .pane_options
                    .get(&("%10".to_string(), "hive-notify-active".to_string())),
                Some(&"%10:new-fire".to_string())
            );
        });
    }

    #[test]
    fn test_cleanup_selected_window_clears_current_token() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        assert!(!cleanup_selected_window("dev:1", "/dev/ttys050"));
        assert!(actions3().is_empty());

        fake_tmux::with_state(|state| {
            state
                .window_options
                .insert("hive-notify-token".to_string(), "%9:old-fire".to_string());
            state
                .window_options
                .insert(NOTIFY_TEXT_KEY.to_string(), "orch: m1".to_string());
            state.pane_options.insert(
                ("%9".to_string(), "hive-notify-active".to_string()),
                "%9:old-fire".to_string(),
            );
            state.panes = vec!["%9".to_string()];
        });

        assert!(cleanup_selected_window("dev:1", "/dev/ttys050"));

        fake_tmux::with_state(|state| {
            assert!(state.window_options.is_empty());
            assert!(state.pane_options.is_empty());
        });
        assert!(run_calls().is_empty());
    }

    #[test]
    fn test_notify_with_workspace_writes_ui_events() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        mock_tmux_basics();
        crate::notify_debug::tests::fake_tmux::reset();

        notify("回来确认", "%9", workspace.to_str().unwrap()).unwrap();

        let log = workspace.join("run").join("notify.jsonl");
        let text = fs::read_to_string(&log).unwrap();
        let events: Vec<String> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(events.contains(&"notify.call".to_string()));
        assert!(events.contains(&"attention.start".to_string()));
        assert!(events.contains(&"attention.done".to_string()));
    }
}
