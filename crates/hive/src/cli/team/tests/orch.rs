use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Default)]
struct Display {
    calls: Vec<Vec<String>>,
    moved: bool,
    shell: bool,
    committed_at_switch: bool,
    tagged_before_registry: bool,
    tags: HashMap<String, String>,
    clients: String,
    failure: &'static str,
    panes: usize,
    linked: bool,
    foreign_session: bool,
    owned_session: bool,
}

fn setup(panes: usize) -> (crate::testkit::DisplayEnv, Rc<RefCell<Display>>) {
    let mut env = display_env_outside();
    env.env.set("TMUX", "/tmp/hive-unit-only,1,0");
    env.env.set("TMUX_PANE", "%0");
    env.env.set("HOME", env._tmp.path());
    env.env.set("CODEX_HOME", env._tmp.path().join("codex"));
    env.env.set("GROK_HOME", env._tmp.path().join("grok"));
    let state = Rc::new(RefCell::new(Display {
        panes,
        ..Default::default()
    }));
    let shared = Rc::clone(&state);
    crate::team::set_fake_tmux_run(|_, _| Ok(tmux::ok_run(0, "", "")));
    crate::team::set_fake_tmux_panes(|_| Vec::new());
    let root = env._tmp.path().to_path_buf();
    tmux::set_run_override(move |args, check, _| {
        let mut state = shared.borrow_mut();
        state.calls.push(args.to_vec());
        let verb = args[0].as_str();
        if (state.failure == verb
            || (state.failure == "pane-bind"
                && verb == "set-option"
                && args.get(4).map(String::as_str) == Some("@hive-team")))
            && check
        {
            return Err(tmux::TmuxError::CalledProcess {
                returncode: 1,
                stderr: "injected".into(),
            });
        }
        let target = args
            .iter()
            .position(|v| v == "-t")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .unwrap_or("");
        let in_team = target == "@1"
            || target == "honey:1"
            || (target == "%0" && state.moved)
            || (target == "%1" && !state.moved);
        let output = match verb {
            "has-session" => {
                return Ok(tmux::ok_run(
                    if state.foreign_session || state.owned_session {
                        0
                    } else {
                        1
                    },
                    "",
                    "",
                ))
            }
            "list-windows" if state.owned_session => "1\thoney".into(),
            "list-clients" => state.clients.clone(),
            "new-window" => {
                state.shell = true;
                "honey:1\t%1".into()
            }
            "respawn-pane" => {
                assert!(state.moved, "shell must start after swap");
                assert!(
                    crate::registry::load("honey").is_some(),
                    "shell must start after registry commit"
                );
                assert_eq!(state.panes, 1, "only a single-pane source needs a shell");
                assert_eq!(target, "%1");
                String::new()
            }
            "new-session" => {
                state.shell = true;
                "%1".into()
            }
            "swap-pane" => {
                state.moved = !state.moved;
                if state.failure == "swap-timeout" && state.moved {
                    return Err(tmux::TmuxError::Timeout);
                }
                if state.failure == "registry" && state.moved {
                    let lock = root.join(".hive/teams/.lock");
                    let _ = std::fs::remove_file(&lock);
                    std::fs::create_dir(lock).unwrap();
                }
                String::new()
            }
            "kill-pane" => {
                assert_eq!(target, "%1", "only the created shell may be killed");
                state.shell = false;
                String::new()
            }
            "switch-client" => {
                state.committed_at_switch = crate::registry::load("honey").is_some();
                String::new()
            }
            "set-option" if args.get(1).map(String::as_str) == Some("-p") => {
                if target == "%0" {
                    if args[4] == "-u" {
                        state.tags.remove(&args[5]);
                    } else {
                        if args[4] == "@hive-team" {
                            state.tagged_before_registry = crate::registry::load("honey").is_none();
                        }
                        state.tags.insert(args[4].clone(), args[5].clone());
                    }
                }
                String::new()
            }
            "show-options" if args.get(1).map(String::as_str) == Some("-p") => {
                let key = args.last().unwrap();
                state.tags.get(key).cloned().unwrap_or_else(|| {
                    if key == "@hive-cli" {
                        "claude".into()
                    } else {
                        String::new()
                    }
                })
            }
            "display-message" => match args.last().unwrap().as_str() {
                "#{window_id}" => if in_team { "@1" } else { "@3" }.into(),
                "#{window_linked}" => if state.linked { "1" } else { "0" }.into(),
                "#{window_panes}" => state.panes.to_string(),
                "#{window_zoomed_flag}" => "0".into(),
                "#{session_name}" => if in_team { "honey" } else { "human" }.into(),
                "#{session_id}" => if in_team { "$1" } else { "$0" }.into(),
                "#{session_name}:#{window_index}" => {
                    if in_team { "honey:1" } else { "human:3" }.into()
                }
                "#{pane_current_path}" => root.display().to_string(),
                "#{@hive-team}" if in_team && state.moved => "honey".into(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        Ok(tmux::ok_run(0, &output, ""))
    });
    (env, state)
}

#[test]
fn test_create_orch_moves_existing_pane_to_team_session() {
    let (_env, state) = setup(1);
    state.borrow_mut().clients = "terminal\t0\t@3\ncontrol\t1\t@3\nelsewhere\t0\t@9".into();
    let _hived = hived_answering_ping("honey");
    let result = create_orch_team("%0", "honey").unwrap();
    assert_eq!(result["window"], "honey:1");
    assert_eq!(result["orch"]["pane"], "%0");
    assert!(result.get("nextStep").is_none());
    let state = state.borrow();
    assert!(state.moved && state.committed_at_switch && state.tagged_before_registry);
    assert!(state
        .calls
        .iter()
        .any(|a| a == &["switch-client", "-c", "terminal", "-t", "honey:1"]));
    assert!(state
        .calls
        .iter()
        .any(|a| a == &["set-option", "-t", "$1", "status", "2"]));
    assert!(!state.calls.iter().any(|a| a.iter().any(|v| v == "$0")));
}

#[test]
fn test_create_orch_preserves_source_window_with_one_pane() {
    let (_env, state) = setup(1);
    let _hived = hived_answering_ping("honey");
    let result = create_orch_team("%0", "honey").unwrap();
    assert!(result["nextStep"]
        .as_str()
        .unwrap()
        .contains("hive attach honey"));
    let state = state.borrow();
    assert!(state.moved && state.shell);
    let creation = state.calls.iter().find(|a| a[0] == "new-session").unwrap();
    assert_eq!(
        creation.last().unwrap(),
        "/bin/sh -c 'exec sleep 2147483647'"
    );
    let respawns: Vec<_> = state
        .calls
        .iter()
        .filter(|a| a[0] == "respawn-pane")
        .collect();
    assert_eq!(respawns.len(), 1);
    assert!(respawns[0]
        .last()
        .unwrap()
        .ends_with("exec \"${SHELL:-/bin/sh}\""));
    assert!(!state
        .calls
        .iter()
        .any(|a| matches!(a[0].as_str(), "kill-pane" | "kill-window" | "kill-session")));
}

#[test]
fn test_create_orch_keeps_other_source_panes() {
    let (_env, state) = setup(3);
    let _hived = hived_answering_ping("honey");
    create_orch_team("%0", "honey").unwrap();
    let state = state.borrow();
    assert!(state.moved && !state.shell);
    assert!(!state.calls.iter().any(|a| a[0] == "respawn-pane"));
    assert_eq!(
        state.calls.iter().filter(|a| a[0] == "kill-pane").count(),
        1
    );
    assert!(!state
        .calls
        .iter()
        .any(|a| a[0] == "kill-window" || a[0] == "kill-session"));
}

#[test]
fn test_create_orch_reuses_binding_without_moving_pane() {
    let (_env, state) = setup(1);
    state.borrow_mut().tags.extend([
        ("@hive-team".into(), "honey".into()),
        ("@hive-agent".into(), "orch".into()),
    ]);
    let result = create_orch_team("%0", "honey").unwrap();
    assert_eq!(result["team"], "honey");
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|a| a[0] == "swap-pane" || a[0] == "new-session"));
}

#[test]
fn test_create_orch_rejects_foreign_session_before_moving() {
    let (_env, state) = setup(1);
    state.borrow_mut().foreign_session = true;
    let error = create_orch_team("%0", "honey").unwrap_err();
    assert!(error.to_string().contains("not owned by Hive"));
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|a| matches!(a[0].as_str(), "swap-pane" | "new-session" | "kill-session")));
}

#[test]
fn test_create_orch_refuses_a_linked_source_window() {
    let (_env, state) = setup(1);
    state.borrow_mut().linked = true;
    assert!(create_orch_team("%0", "honey")
        .unwrap_err()
        .to_string()
        .contains("linked across sessions"));
    assert!(!state.borrow().calls.iter().any(|a| a[0] == "new-session"));
}

#[test]
fn test_create_orch_rolls_back_before_registry_commit() {
    let (env, state) = setup(1);
    state.borrow_mut().failure = "registry";
    let error = create_orch_team("%0", "honey").unwrap_err();
    assert!(!error.to_string().is_empty());
    let state = state.borrow();
    assert!(!state.moved && !state.shell);
    assert_eq!(
        state.tags.get("@hive-cli").map(String::as_str),
        Some("claude")
    );
    assert!(state.tags.get("@hive-team").is_none());
    assert!(state.tags.get("@hive-agent").is_none());
    assert!(state.tags.get("@hive-role").is_none());
    assert!(!env._tmp.path().join(".hive/contexts/pane-0.json").exists());
    assert!(crate::registry::load("honey").is_none());
    assert_eq!(
        state.calls.iter().filter(|a| a[0] == "swap-pane").count(),
        2
    );
}

#[test]
fn test_create_orch_removes_only_the_new_shell_when_swap_fails() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "swap-pane";
    assert!(create_orch_team("%0", "honey").is_err());
    assert!(!state.borrow().moved && !state.borrow().shell);
    assert!(crate::registry::load("honey").is_none());
}

#[test]
fn test_create_orch_leaves_source_when_session_creation_fails() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "new-session";
    assert!(create_orch_team("%0", "honey").is_err());
    assert!(!state.borrow().moved && !state.borrow().shell);
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|a| a[0].starts_with("kill-")));
}

#[test]
fn test_create_orch_keeps_committed_team_when_client_switch_fails() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "switch-client";
    state.borrow_mut().clients = "terminal\t0\t@3".into();
    let _hived = hived_answering_ping("honey");
    let result = create_orch_team("%0", "honey").unwrap();
    assert!(result["nextStep"]
        .as_str()
        .unwrap()
        .contains("hive attach honey"));
    assert!(crate::registry::load("honey").is_some());
    assert!(state.borrow().moved);
    assert_eq!(
        state
            .borrow()
            .calls
            .iter()
            .filter(|a| a[0] == "swap-pane")
            .count(),
        1
    );
}

#[test]
fn test_create_orch_selects_only_unambiguous_source_client() {
    let (_env, state) = setup(1);
    for (clients, expected) in [
        ("", None),
        ("ctl\t1\t@3", None),
        ("one\t0\t@3\nctl\t1\t@3\nother\t0\t@4", Some("one")),
        ("one\t0\t@3\ntwo\t0\t@3", None),
    ] {
        state.borrow_mut().clients = clients.into();
        assert_eq!(tmux::sole_window_client("@3").as_deref(), expected);
    }
}

#[test]
fn test_create_orch_recovers_a_swap_that_completed_before_timeout() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "swap-timeout";
    assert!(create_orch_team("%0", "honey").is_err());
    assert!(!state.borrow().moved && !state.borrow().shell);
    assert!(crate::registry::load("honey").is_none());
    assert_eq!(
        state
            .borrow()
            .calls
            .iter()
            .filter(|a| a[0] == "swap-pane")
            .count(),
        2
    );
}

#[test]
fn test_create_orch_rolls_back_when_status_install_fails() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "bind-key";
    assert!(create_orch_team("%0", "honey").is_err());
    assert!(!state.borrow().moved && !state.borrow().shell);
    assert!(crate::registry::load("honey").is_none());
}

#[test]
fn test_create_orch_restores_tags_after_a_partial_bind() {
    let (_env, state) = setup(1);
    state.borrow_mut().failure = "pane-bind";
    assert!(create_orch_team("%0", "honey").is_err());
    let state = state.borrow();
    assert!(!state.moved && !state.shell);
    assert!(crate::registry::load("honey").is_none());
    assert_eq!(
        state.tags.get("@hive-cli").map(String::as_str),
        Some("claude")
    );
    for key in ["@hive-team", "@hive-agent", "@hive-role"] {
        assert!(!state.tags.contains_key(key));
    }
}

#[test]
fn test_create_orch_existing_team_session_starts_a_placeholder_window() {
    let (env, state) = setup(1);
    state.borrow_mut().owned_session = true;
    let _hived = hived_answering_ping("honey");
    create_orch_team("%0", "honey").unwrap();
    let state = state.borrow();
    assert!(!state.calls.iter().any(|a| a[0] == "new-session"));
    let creation = state.calls.iter().find(|a| a[0] == "new-window").unwrap();
    assert_eq!(
        creation.last().unwrap(),
        "/bin/sh -c 'exec sleep 2147483647'"
    );
    let cwd = creation.iter().position(|v| v == "-c").unwrap();
    assert_eq!(creation[cwd + 1], env._tmp.path().to_str().unwrap());
    assert_eq!(
        state
            .calls
            .iter()
            .filter(|a| a[0] == "respawn-pane")
            .count(),
        1
    );
}

#[test]
fn test_create_orch_rollback_never_starts_a_shell() {
    for failure in [
        "new-session",
        "bind-key",
        "swap-pane",
        "swap-timeout",
        "pane-bind",
        "registry",
    ] {
        let (_env, state) = setup(1);
        state.borrow_mut().failure = failure;
        assert!(create_orch_team("%0", "honey").is_err(), "{failure}");
        assert!(
            !state.borrow().calls.iter().any(|a| a[0] == "respawn-pane"),
            "{failure}"
        );
    }
}
