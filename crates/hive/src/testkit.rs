//! Crate-wide test fixtures shared by more than one module's tests: a
//! registry-backed team, the tmux double the display verbs run against, and
//! the env/hived stand-ins those verbs cross. Module-local fixtures stay in
//! their module's `tests`.

use serde_json::{json, Map, Value};

use crate::team::{created_at_key, Team};
use crate::testenv::EnvGuard;

pub(crate) fn registry_team(name: &str, created_at: f64, members: &[&str]) -> Team {
    let rows: Vec<Map<String, Value>> = members
        .iter()
        .map(|n| {
            let mut m = Map::new();
            m.insert("name".to_string(), Value::String((*n).to_string()));
            m
        })
        .collect();
    crate::registry::record_team(name, "/ws", &created_at_key(created_at), &rows, "").unwrap();
    Team {
        name: name.to_string(),
        created_at,
        ..Default::default()
    }
}

pub(crate) fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Temp `$HIVE_HOME` + an inside-tmux env, held for the test's lifetime.
pub(crate) struct DisplayEnv {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) env: EnvGuard,
}

pub(crate) fn display_env() -> DisplayEnv {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    // Claude's pane→job records (`Team::load` reads them for claude member
    // panes) come from a throwaway tree, never the developer's own.
    env.set("CLAUDE_HOME", tmp.path().join(".claude"));
    env.set("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
    // Inside tmux: the jump must never reach exec_attach, which would
    // replace the test process with `tmux attach`.
    env.set("TMUX", "/tmp/hive-test-tmux,1,0");
    env.set("TMUX_PANE", "%0");
    DisplayEnv { _tmp: tmp, env }
}

/// `display_env` for a caller outside tmux with no engine identity at all.
pub(crate) fn display_env_outside() -> DisplayEnv {
    let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    env.set("CLAUDE_HOME", tmp.path().join(".claude"));
    env.set("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
    DisplayEnv { _tmp: tmp, env }
}

pub(crate) fn member_row(name: &str, cli: &str, session_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("name".to_string(), Value::from(name));
    m.insert("cli".to_string(), Value::from(cli));
    m.insert("sessionId".to_string(), Value::from(session_id));
    m.insert("cwd".to_string(), Value::from("/tmp"));
    m
}

pub(crate) type Argv = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

/// A tmux double that answers everything the display verbs ask and records
/// every argv. `windows` is the `list-windows -a` stdout; `panes` the pane
/// rows the team window already has (`PANE_BASE_FMT` order, tab-joined).
/// The caller's own session `dev` exists.
///
/// Two seams, because `find_team_window` resolves tmux through `team/mod.rs`'s
/// own fake (`team::tests::fake_tmux`) in test builds while `team_display.rs`
/// calls the real module. Only the real module's argv is recorded — which is
/// the half that writes.
pub(crate) fn fake_tmux(windows: &'static str, panes: &'static [&'static str]) -> Argv {
    fake_tmux_sessions(windows, panes, &[], &["dev"])
}

/// `fake_tmux` whose `show-options -p` also answers pane tags:
/// `(pane, key, value)` rows, `key` without the `@` (`hive-team`). Tagging
/// the caller's own pane (`%0` under `display_env`) is what lets a verb with
/// no team argument discover its team through the binding ladder.
pub(crate) fn fake_tmux_tagged(
    windows: &'static str,
    panes: &'static [&'static str],
    tags: &'static [(&'static str, &'static str, &'static str)],
) -> Argv {
    fake_tmux_sessions(windows, panes, tags, &["dev"])
}

/// A window the double built (`new-session` / `new-window`): it joins the
/// `list-windows` answer once `@hive-team` is set on it, and `team/mod.rs`'s seams
/// list its first pane, so a team re-loaded after a heal anchors on the
/// window the heal built rather than the caller's pane.
pub(crate) struct BuiltWindow {
    target: String,
    first_pane: String,
    team: String,
}

/// The full double: `sessions` are the live session names, resolved the
/// way tmux resolves `-t` — exact name, else the first name it prefixes;
/// only `=name` is exact — until a `new-session` creates one. `new-session`
/// answers a fresh pane; `display-message` answers for the last session
/// created here (else `dev`), with the first window at index 1 on purpose —
/// base-index is not always 0. `tags` are `(target, key, value)` rows the
/// double answers for pane tags (`show-options -p`) and window options
/// (`display-message -t <window> #{@key}`) alike; what the verbs write
/// (`set-option -p`, `set-window-option @…`) is answered back the same way,
/// and a pane without a fixture row (a split, a joined-back mirror) gets
/// its full-format `list-panes` row from its tags. `kill-pane` drops the
/// pane from every listing; `break-pane` parks it (it leaves the window's
/// listings, `list-panes -a` shows it in the hidden window `@9`) and
/// `join-pane` brings it back at the front. A pane seeded parked carries a
/// `(pane, "hive-hidden", team)` tag.
pub(crate) fn fake_tmux_sessions(
    windows: &'static str,
    panes: &'static [&'static str],
    tags: &'static [(&'static str, &'static str, &'static str)],
    sessions: &'static [&'static str],
) -> Argv {
    let built: std::rc::Rc<std::cell::RefCell<Vec<BuiltWindow>>> = Default::default();
    let listing = {
        let built = std::rc::Rc::clone(&built);
        move || -> String {
            let mut out = windows.to_string();
            for w in built.borrow().iter().filter(|w| !w.team.is_empty()) {
                out.push_str(&format!("{}\t@7\t{}\t\t\t\n", w.target, w.team));
            }
            out
        }
    };
    {
        let listing = listing.clone();
        crate::team::set_fake_tmux_run(move |args, _check| {
            Ok(crate::tmux::Run {
                returncode: 0,
                stdout: if args[0] == "list-windows" {
                    listing()
                } else {
                    windows.to_string()
                },
                stderr: String::new(),
            })
        });
    }
    {
        let built = std::rc::Rc::clone(&built);
        crate::team::set_fake_tmux_panes(move |target| {
            built
                .borrow()
                .iter()
                .filter(|w| w.target == target)
                .map(|w| crate::tmux::PaneInfo {
                    pane_id: w.first_pane.clone(),
                    ..Default::default()
                })
                .collect()
        });
    }
    let argv: Argv = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = std::rc::Rc::clone(&argv);
    let mut live: Vec<String> = panes
        .iter()
        .map(|row| row.split('\t').next().unwrap_or_default().to_string())
        .collect();
    // (pane, team) parked in the hidden window `@9`.
    let mut hidden: Vec<(String, String)> = tags
        .iter()
        .filter(|(_, key, _)| *key == "hive-hidden")
        .map(|(pane, _, team)| ((*pane).to_string(), (*team).to_string()))
        .collect();
    let mut written: std::collections::HashMap<(String, String), String> = Default::default();
    // Fresh panes number after the fixture's, as tmux would.
    let mut next_pane = live
        .iter()
        .filter_map(|p| p.trim_start_matches('%').parse::<usize>().ok())
        .max()
        .map_or(1, |n| n + 1);
    let mut live_sessions: Vec<String> = sessions.iter().map(|s| s.to_string()).collect();
    let mut created_session: Option<String> = None;
    crate::tmux::set_run_override(move |args, _check, _timeout| {
        recorded.borrow_mut().push(args.to_vec());
        let mut returncode = 0;
        let session = created_session.clone().unwrap_or_else(|| "dev".to_string());
        // What a verb wrote outranks the fixture: the fixture is the state
        // before the verb ran.
        let tag_for = |target: &str, key: &str| -> String {
            written
                .get(&(target.to_string(), key.to_string()))
                .cloned()
                .or_else(|| {
                    tags.iter()
                        .find(|(t, k, _)| *t == target && *k == key)
                        .map(|(_, _, v)| (*v).to_string())
                })
                .unwrap_or_default()
        };
        // A pane's tags as the full `list-panes` format reads them: the
        // fixture row when there is one, else its (written) tags.
        let full_row = |pane: &str| -> String {
            if let Some(row) = panes
                .iter()
                .find(|r| r.split('\t').next().unwrap_or_default() == pane)
            {
                return (*row).to_string();
            }
            let tag = |key: &str| tag_for(pane, key);
            format!(
                "{pane}\t\t\t{}\t{}\t{}\t{}\t{}",
                tag("hive-role"),
                tag("hive-agent"),
                tag("hive-team"),
                tag("hive-cli"),
                tag("hive-group")
            )
        };
        let role_of = |pane: &str| -> String {
            full_row(pane)
                .split('\t')
                .nth(3)
                .unwrap_or_default()
                .to_string()
        };
        let out = match args[0].as_str() {
            "list-windows"
                if args.last().map(String::as_str) == Some("#{window_id}\t#{@hive-hidden}") =>
            {
                hidden
                    .iter()
                    .map(|(_, team)| format!("@9\t{team}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "list-windows" => listing(),
            "has-session" => {
                let target = args.get(2).cloned().unwrap_or_default();
                let found = match target.strip_prefix('=') {
                    Some(exact) => live_sessions.iter().any(|s| s == exact),
                    None => live_sessions
                        .iter()
                        .any(|s| *s == target || s.starts_with(&target)),
                };
                if !found {
                    returncode = 1;
                }
                String::new()
            }
            "new-session" => {
                // new-session -d -s <name> …
                let name = args.get(3).cloned().unwrap_or_default();
                live_sessions.push(name.clone());
                created_session = Some(name.clone());
                let pane = format!("%{next_pane}");
                next_pane += 1;
                live.push(pane.clone());
                built.borrow_mut().push(BuiltWindow {
                    target: format!("{name}:1"),
                    first_pane: pane.clone(),
                    team: String::new(),
                });
                pane
            }
            "new-window" => {
                // new-window -t [=]<session>: …
                let target = args
                    .get(2)
                    .map(|t| t.trim_start_matches('=').trim_end_matches(':').to_string())
                    .unwrap_or_default();
                let pane = format!("%{next_pane}");
                next_pane += 1;
                live.push(pane.clone());
                built.borrow_mut().push(BuiltWindow {
                    target: format!("{target}:2"),
                    first_pane: pane.clone(),
                    team: String::new(),
                });
                format!("{target}:2\t{pane}")
            }
            "set-window-option" => {
                // set-window-option -t <window> @<option> <value>
                let target = args.get(2).cloned().unwrap_or_default();
                if let Some(key) = args.get(3).and_then(|opt| opt.strip_prefix('@')) {
                    let value = args.get(4).cloned().unwrap_or_default();
                    if key == "hive-team" {
                        for w in built.borrow_mut().iter_mut().filter(|w| w.target == target) {
                            w.team = value.clone();
                        }
                    }
                    if key == "hive-hidden" {
                        // The tag lands on the window a break-pane just
                        // made: its pane is the one parked without a team.
                        for (_, team) in hidden.iter_mut().filter(|(_, t)| t.is_empty()) {
                            *team = value.clone();
                        }
                    }
                    written.insert((target, key.to_string()), value);
                }
                String::new()
            }
            "set-option" if args.get(1).map(String::as_str) == Some("-p") => {
                // set-option -p -t <pane> [-u] @<key> [<value>]
                let pane = args.get(3).cloned().unwrap_or_default();
                if args.get(4).map(String::as_str) == Some("-u") {
                    if let Some(key) = args.get(5).and_then(|opt| opt.strip_prefix('@')) {
                        written.remove(&(pane, key.to_string()));
                    }
                } else if let Some(key) = args.get(4).and_then(|opt| opt.strip_prefix('@')) {
                    let value = args.get(5).cloned().unwrap_or_default();
                    written.insert((pane, key.to_string()), value);
                }
                String::new()
            }
            // Session options (the status bar) and key bindings: recorded,
            // nothing to answer.
            "set-option" | "bind-key" => String::new(),
            "break-pane" => {
                // break-pane -s <pane> -d [-t <session>:] -n <name> -P -F …
                let pane = args.get(2).cloned().unwrap_or_default();
                live.retain(|p| *p != pane);
                hidden.push((pane.clone(), String::new()));
                let target = args
                    .iter()
                    .position(|a| a == "-t")
                    .and_then(|i| args.get(i + 1))
                    .map(|t| t.trim_start_matches('=').trim_end_matches(':').to_string())
                    .unwrap_or_else(|| session.clone());
                format!("{target}:9\t{pane}")
            }
            "join-pane" => {
                // join-pane -h -b -d -s <src> -t <dst>
                let src = args.get(5).cloned().unwrap_or_default();
                hidden.retain(|(p, _)| *p != src);
                live.insert(0, src);
                String::new()
            }
            "split-window" => {
                let pane = format!("%{next_pane}");
                next_pane += 1;
                live.push(pane.clone());
                pane
            }
            "kill-pane" => {
                let pane = args.get(2).cloned().unwrap_or_default();
                live.retain(|p| *p != pane);
                String::new()
            }
            "list-panes" if args.get(1).map(String::as_str) == Some("-a") => {
                // list-panes -a -F '#{pane_id}\t#{window_id}\t#{@hive-role}'
                live.iter()
                    .map(|p| format!("{p}\t@7\t{}", role_of(p)))
                    .chain(
                        hidden
                            .iter()
                            .map(|(p, _)| format!("{p}\t@9\t{}", role_of(p))),
                    )
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "list-panes" => {
                let fmt = args.last().cloned().unwrap_or_default();
                if fmt == "#{pane_id}" {
                    live.join("\n")
                } else {
                    live.iter()
                        .map(|p| full_row(p))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "show-options" => {
                // show-options -p -v -t <pane> @<key>
                let pane = args.get(4).map(String::as_str).unwrap_or_default();
                let key = args
                    .get(5)
                    .and_then(|opt| opt.strip_prefix('@'))
                    .unwrap_or_default();
                tag_for(pane, key)
            }
            "display-message" => match args.last().map(String::as_str).unwrap_or_default() {
                "#{session_name}" => session,
                "#{session_name}:#{window_index}" => format!("{session}:1"),
                "#{session_id}" => "$1".to_string(),
                "#{window_id}" => "@7".to_string(),
                "#{window_width}\t#{window_height}" => "200\t50".to_string(),
                // display-message -t <window> -p #{@key}
                fmt if fmt.starts_with("#{@") => tag_for(
                    args.get(2).map(String::as_str).unwrap_or_default(),
                    fmt.trim_start_matches("#{@").trim_end_matches('}'),
                ),
                _ => String::new(),
            },
            _ => String::new(),
        };
        Ok(crate::tmux::Run {
            returncode,
            stdout: format!("{out}\n"),
            stderr: String::new(),
        })
    });
    argv
}

pub(crate) fn verbs(argv: &Argv) -> Vec<String> {
    argv.borrow().iter().map(|a| a[0].clone()).collect()
}

pub(crate) fn count(argv: &Argv, verb: &str) -> usize {
    verbs(argv).iter().filter(|v| *v == verb).count()
}

pub(crate) fn has_row(argv: &Argv, row: &[&str]) -> bool {
    argv.borrow().iter().any(|a| a[..] == *row)
}

/// Healthy identity for the hived hook's ping, so `start_team_hived`
/// believes a hived is up and starts none. Every other request still goes
/// to the real workspace socket: unanswered when nothing listens there,
/// answered by `fake_hived` when a test binds it.
pub(crate) fn hived_answering_ping(team: &str) -> crate::hived::testhook::Guard {
    let team = team.to_string();
    let request_ping = std::sync::Arc::new(move |_ws: &str| {
        let mut identity = Map::new();
        identity.insert("ok".to_string(), Value::Bool(true));
        identity.insert(
            "apiVersion".to_string(),
            Value::from(crate::hived::HIVED_API_VERSION),
        );
        identity.insert(
            "buildHash".to_string(),
            Value::from(crate::hived::hived_build_hash()),
        );
        identity.insert("team".to_string(), Value::from(team.clone()));
        Some(identity)
    });
    crate::hived::testhook::install(crate::hived::testhook::Hook {
        request_ping: Some(request_ping),
        ..Default::default()
    })
}

pub(crate) fn team_dir(env: &DisplayEnv, team: &str) -> std::path::PathBuf {
    env._tmp.path().join(".hive").join("teams").join(team)
}

/// This process is a live Claude session `me` (sessionId `s-me`): its
/// inbox socket names its registration, whose sessionId is an interactive
/// one — no bg job row, so the mirror lane is `hive view`.
pub(crate) fn claude_session_me(
    env: &mut DisplayEnv,
) -> crate::adapters::claude_bg::testhook::Guard {
    let sessions = env._tmp.path().join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("me.json"),
        json!({
            "name": "me",
            "pid": std::process::id(),
            "messagingSocketPath": "/tmp/me.sock",
            "sessionId": "s-me",
        })
        .to_string(),
    )
    .unwrap();
    env.env.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/me.sock");
    crate::adapters::claude_bg::testhook::install(crate::adapters::claude_bg::testhook::Hook {
        list_jobs_rows: Some(Some(Vec::new())),
        ..Default::default()
    })
}
