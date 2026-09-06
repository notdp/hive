// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

use serde_json::{json, Map, Value};

use super::*;
use crate::team::Team;
use crate::testenv::EnvGuard;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_attach_backfills_only_missing_attachable_members() {
    let rendered: std::collections::HashSet<String> =
        ["orch".to_string(), "scout".to_string()].into();
    let picked = _members_to_backfill(
        &rendered,
        vec![
            member_row("orch", "claude", "sid-1"),  // already rendered
            member_row("scout", "claude", "sid-2"), // already rendered
            member_row("sage", "grok", "sid-3"),    // missing -> backfill
            member_row("ghost", "grok", ""),        // no engine identity
            member_row("shelly", "bash", "sid-4"),  // not an agent CLI
        ],
    );
    let names: Vec<String> = picked.iter().map(|m| map_str(m, "name")).collect();
    assert_eq!(names, vec!["sage".to_string()]);
}

// --- attach (heal: rebuild a missing window, backfill panes, then jump) ---

/// Temp `$HIVE_HOME` + an inside-tmux env, held for the test's lifetime.
struct DisplayEnv {
    _tmp: tempfile::TempDir,
    env: EnvGuard,
}

fn display_env() -> DisplayEnv {
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
fn display_env_outside() -> DisplayEnv {
    let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    env.set("CLAUDE_HOME", tmp.path().join(".claude"));
    env.set("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
    DisplayEnv { _tmp: tmp, env }
}

fn member_row(name: &str, cli: &str, session_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("name".to_string(), Value::from(name));
    m.insert("cli".to_string(), Value::from(cli));
    m.insert("sessionId".to_string(), Value::from(session_id));
    m.insert("cwd".to_string(), Value::from("/tmp"));
    m
}

type Argv = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

/// A tmux double that answers everything the display verbs ask and records
/// every argv. `windows` is the `list-windows -a` stdout; `panes` the pane
/// rows the team window already has (`_PANE_BASE_FMT` order, tab-joined).
/// The caller's own session `dev` exists.
///
/// Two seams, because `_find_team_window` resolves tmux through `team.rs`'s
/// own fake in test builds while `attach.rs` calls the real module. Only the
/// real module's argv is recorded — which is the half that writes.
fn fake_tmux(windows: &'static str, panes: &'static [&'static str]) -> Argv {
    fake_tmux_sessions(windows, panes, &[], &["dev"])
}

/// `fake_tmux` whose `show-options -p` also answers pane tags:
/// `(pane, key, value)` rows, `key` without the `@` (`hive-team`). Tagging
/// the caller's own pane (`%0` under `display_env`) is what lets a verb with
/// no team argument discover its team through the binding ladder.
fn fake_tmux_tagged(
    windows: &'static str,
    panes: &'static [&'static str],
    tags: &'static [(&'static str, &'static str, &'static str)],
) -> Argv {
    fake_tmux_sessions(windows, panes, tags, &["dev"])
}

/// A window the double built (`new-session` / `new-window`): it joins the
/// `list-windows` answer once `@hive-team` is set on it, and team.rs's seams
/// list its first pane, so a team re-loaded after a heal anchors on the
/// window the heal built rather than the caller's pane.
struct BuiltWindow {
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
fn fake_tmux_sessions(
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
        crate::team::_set_fake_tmux_run(move |args, _check| {
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
        crate::team::_set_fake_tmux_panes(move |target| {
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
    crate::tmux::_set_run_override(move |args, _check, _timeout| {
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

fn verbs(argv: &Argv) -> Vec<String> {
    argv.borrow().iter().map(|a| a[0].clone()).collect()
}

fn count(argv: &Argv, verb: &str) -> usize {
    verbs(argv).iter().filter(|v| *v == verb).count()
}

fn has_row(argv: &Argv, row: &[&str]) -> bool {
    argv.borrow().iter().any(|a| a[..] == *row)
}

#[test]
fn test_attach_names_the_missing_team_before_looking_at_tmux() {
    let _env = display_env();
    let argv = fake_tmux("", &[]);

    let message = _team_entry("ghost").unwrap_err();

    assert!(message.contains("hive ls"), "{message}");
    assert!(argv.borrow().is_empty());
}

#[test]
fn test_attach_with_a_window_switches_the_client() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%1\t[orch]\tgrok\tagent\torch\thoney\tgrok\t"],
    );

    attach_cmd("honey");

    let recorded = argv.borrow();
    // switch-client moves *this* client; select-window would only retarget
    // the window's own session and leave the caller where it was.
    assert!(recorded
        .iter()
        .any(|a| a[..] == ["switch-client", "-t", "dev:2"]));
    assert!(recorded.iter().all(|a| a[0] != "select-window"));
    // Every member has its pane: nothing to build.
    assert!(recorded
        .iter()
        .all(|a| !matches!(a[0].as_str(), "new-window" | "split-window" | "send-keys")));
}

#[test]
fn test_attach_without_a_window_rebuilds_it_and_records_the_display() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
            member_row("ghost", "grok", ""),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux("", &[]);

    attach_cmd("honey");

    // One window — in the caller's own session, since the caller is inside
    // tmux — and one split for the second attachable member; the member
    // with no engine identity gets no pane.
    assert_eq!(count(&argv, "new-window"), 1);
    assert!(has_row(
        &argv,
        &[
            "new-window",
            "-t",
            "dev:",
            "-d",
            "-n",
            "honey",
            "-c",
            "/tmp",
            "-P",
            "-F",
            "#{session_name}:#{window_index}\t#{pane_id}",
        ]
    ));
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
    // The freshly built window id lands in the registry's display cache.
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
    // A window in the caller's own session gets no status bar and no
    // binding: their status line is theirs.
    assert_eq!(count(&argv, "bind-key"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "set-option" && a.get(3).map(String::as_str) == Some("status"))));
}

#[test]
fn test_attach_with_a_window_adds_a_pane_for_a_member_spawned_after_it() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "@7",
    )
    .unwrap();
    // The window shows `orch` only — `sage` was spawned after it was built.
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%1\t[orch]\tgrok\tagent\torch\thoney\tgrok\t"],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "new-window"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    // The new pane runs sage's own viewer, not orch's.
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.last().is_some_and(|text| text.contains("sid-sage"))));
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_outside_tmux_builds_the_team_session() {
    let _env = display_env_outside();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    // Not `attach_cmd`: outside tmux it would exec `tmux attach`.
    let (window, built) = _ensure_team_display(&crate::registry::load("honey").unwrap());

    assert!(built);
    // The window's index is read back from tmux, never assumed to be 0.
    assert_eq!(window, "honey:1");
    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "honey",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    assert!(has_row(&argv, &["rename-window", "-t", "honey:1", "honey"]));
    assert_eq!(count(&argv, "new-window"), 0);
    // The one split hangs off the pane `new-session` handed back, so the
    // second member lands in the team session and nowhere else.
    let splits: Vec<Vec<String>> = argv
        .borrow()
        .iter()
        .filter(|a| a[0] == "split-window")
        .cloned()
        .collect();
    assert_eq!(splits.len(), 1, "{splits:?}");
    assert_eq!(&splits[0][..3], ["split-window", "-t", "%1"]);
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "honey:1", "@hive-team", "honey"]
    ));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "honey:1", "@hive-built", "1"]
    ));
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
}

#[test]
fn test_attach_heal_outside_tmux_reuses_a_session_named_after_the_team() {
    let _env = display_env_outside();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux_sessions("", &[], &[], &["honey"]);

    let (_window, built) = _ensure_team_display(&crate::registry::load("honey").unwrap());

    assert!(built);
    assert_eq!(count(&argv, "new-session"), 0);
    assert_eq!(count(&argv, "new-window"), 1);
    let new_window = argv
        .borrow()
        .iter()
        .find(|a| a[0] == "new-window")
        .cloned()
        .unwrap();
    assert_eq!(&new_window[..3], ["new-window", "-t", "=honey:"]);
    assert!(new_window.windows(2).any(|pair| pair == ["-n", "honey"]));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "honey:2", "@hive-built", "1"]
    ));
}

#[test]
fn test_team_session_is_matched_by_exact_name_never_by_prefix() {
    let _env = display_env_outside();
    // A stranger's session whose name merely starts with the team name: a
    // bare `-t hornet` would resolve to it and put the team window there.
    let argv = fake_tmux_sessions("", &[], &[], &["hornet-x"]);

    let (window, first_pane, created) = _new_team_session_window("hornet").unwrap();

    assert!(created);
    assert_eq!(window, "hornet:1");
    assert_eq!(first_pane, "%1");
    assert!(has_row(&argv, &["has-session", "-t", "=hornet"]));
    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "hornet",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    assert_eq!(count(&argv, "new-window"), 0);
}

// --- flow rig --down: the team session by exact name ---------------------

#[test]
fn test_rig_down_without_a_rig_never_kills_a_prefix_matched_session() {
    let _env = display_env_outside();
    let argv = fake_tmux_sessions("", &[], &[], &["abc-keep"]);

    let err = crate::flow_rig::rig_down("abc").unwrap_err().to_string();

    assert!(err.contains("no rig named 'abc'"), "{err}");
    assert_eq!(count(&argv, "kill-session"), 0);
    assert_eq!(count(&argv, "kill-window"), 0);
}

#[test]
fn test_rig_down_kills_the_team_window_and_the_exact_session() {
    let _env = display_env_outside();
    crate::registry::record_team("abc", "", "100.0", &[], "@7").unwrap();
    let argv = fake_tmux_sessions(
        "abc:1	@7	abc			
",
        &[],
        &[("abc:1", "hive-built", "1")],
        &["abc", "abc-keep"],
    );

    crate::flow_rig::rig_down("abc").unwrap();

    assert!(crate::registry::load("abc").is_none());
    assert!(has_row(&argv, &["kill-window", "-t", "@7"]));
    // Every session target is exact: `abc-keep` is never in reach.
    let session_targets: Vec<String> = argv
        .borrow()
        .iter()
        .filter(|a| matches!(a[0].as_str(), "has-session" | "kill-session"))
        .map(|a| a[2].clone())
        .collect();
    assert!(!session_targets.is_empty());
    assert!(
        session_targets.iter().all(|t| t == "=abc"),
        "{session_targets:?}"
    );
}

// --- delete: hive closes the window it built, never the caller's ---------

fn team_on_a_built_window() {
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "")],
        "@7",
    )
    .unwrap();
}

#[test]
fn test_delete_from_inside_the_team_window_leaves_it_to_the_caller() {
    let env = display_env();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    team_on_a_built_window();
    // The caller's own window is the team window (`display-message
    // #{window_id}` answers `@7` for the caller's pane too).
    let argv = fake_tmux_sessions(
        "honey:1	@7	honey			
",
        &[],
        &[("honey:1", "hive-built", "1")],
        &["dev", "honey"],
    );

    core_cmds::_delete_team("honey", &ws, false).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert_eq!(count(&argv, "kill-window"), 0);
}

#[test]
fn test_delete_from_outside_closes_the_window_hive_built() {
    let env = display_env_outside();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    team_on_a_built_window();
    let argv = fake_tmux_sessions(
        "honey:1	@7	honey			
",
        &[],
        &[("honey:1", "hive-built", "1")],
        &["honey"],
    );

    core_cmds::_delete_team("honey", &ws, false).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert!(has_row(&argv, &["kill-window", "-t", "@7"]));
}

#[test]
fn test_delete_leaves_a_window_the_callers_session_lent() {
    let env = display_env_outside();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    team_on_a_built_window();
    // An in-tmux create bound the human's own window: no `@hive-built`.
    let argv = fake_tmux_sessions(
        "dev:2	@7	honey			
",
        &[],
        &[],
        &["dev"],
    );

    core_cmds::_delete_team("honey", &ws, false).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert_eq!(count(&argv, "kill-window"), 0);
}

#[test]
fn test_delete_kills_the_hidden_mirror_window_before_the_team_window() {
    let env = display_env_outside();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    team_on_a_built_window();
    // The orch mirror is parked (`hive mirror off`): its hidden window
    // would keep the team session alive after the team window closes.
    let argv = fake_tmux_sessions(
        "honey:1	@7	honey			
",
        &[],
        &[
            ("honey:1", "hive-built", "1"),
            ("%5", "hive-hidden", "honey"),
        ],
        &["honey"],
    );

    core_cmds::_delete_team("honey", &ws, false).unwrap();

    let kills: Vec<Vec<String>> = argv
        .borrow()
        .iter()
        .filter(|a| a[0] == "kill-window")
        .cloned()
        .collect();
    assert_eq!(
        kills,
        vec![
            args(&["kill-window", "-t", "@9"]),
            args(&["kill-window", "-t", "@7"]),
        ]
    );
}

fn team_on_its_own_dir(env: &DisplayEnv) -> std::path::PathBuf {
    let ws = team_dir(env, "honey");
    crate::registry::record_team(
        "honey",
        ws.to_str().unwrap(),
        "100.0",
        &[member_row("orch", "claude", "")],
        "@7",
    )
    .unwrap();
    std::fs::create_dir_all(ws.join("run")).unwrap();
    std::fs::create_dir_all(ws.join("artifacts")).unwrap();
    std::fs::write(ws.join("hive.db"), "bus").unwrap();
    ws
}

#[test]
fn test_delete_without_the_flag_removes_only_team_json_from_the_team_dir() {
    let env = display_env_outside();
    let ws = team_on_its_own_dir(&env);
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_delete_team("honey", "", false).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert!(!ws.join("team.json").exists());
    assert!(ws.join("hive.db").is_file());
    assert!(ws.join("run").is_dir());
    assert!(ws.join("artifacts").is_dir());
}

#[test]
fn test_delete_with_the_flag_removes_the_whole_team_dir() {
    let env = display_env_outside();
    let ws = team_on_its_own_dir(&env);
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_delete_team("honey", "", true).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert!(!ws.exists(), "{}", ws.display());
    // the store itself stays
    assert!(env._tmp.path().join(".hive").join("teams").is_dir());
}

#[test]
fn test_delete_without_an_entry_ignores_a_workspace_named_by_the_environment() {
    let mut env = display_env_outside();
    let stranger = env._tmp.path().join("stranger");
    std::fs::create_dir_all(stranger.join("artifacts")).unwrap();
    env.env.set("HIVE_WORKSPACE", &stranger);
    env.env.set("CR_WORKSPACE", &stranger);
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_delete_team("honey", "", true).unwrap();

    assert!(stranger.join("artifacts").is_dir());
    assert!(!team_dir(&env, "honey").exists());
}

#[test]
fn test_delete_never_removes_an_external_workspace_without_the_flag() {
    let env = display_env_outside();
    let external = env._tmp.path().join("elsewhere");
    std::fs::create_dir_all(external.join("artifacts")).unwrap();
    std::fs::write(external.join("hive.db"), "bus").unwrap();
    crate::registry::record_team(
        "honey",
        external.to_str().unwrap(),
        "100.0",
        &[member_row("orch", "claude", "")],
        "@7",
    )
    .unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_delete_team("honey", "", false).unwrap();

    assert!(crate::registry::load("honey").is_none());
    assert!(external.join("hive.db").is_file());
    assert!(external.join("artifacts").is_dir());
    // the team dir held only the entry, so it is gone
    assert!(!team_dir(&env, "honey").exists());

    // with the flag, the external workspace goes too
    crate::registry::record_team("honey", external.to_str().unwrap(), "200.0", &[], "@7").unwrap();
    core_cmds::_delete_team("honey", "", true).unwrap();
    assert!(!external.exists());
    assert!(!team_dir(&env, "honey").exists());
}

#[test]
fn test_attach_heal_builds_a_window_for_a_team_with_no_attachable_member() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "")],
        "",
    )
    .unwrap();
    let argv = fake_tmux("", &[]);

    let (_window, built) = _ensure_team_display(&crate::registry::load("honey").unwrap());

    // The window exists for the team, not for its members: nobody rides a
    // pane, so the first pane stays a shell and no viewer is launched.
    assert!(built);
    assert_eq!(count(&argv, "new-window"), 1);
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "send-keys"), 0);
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
}

// --- tests/unit/test_launcher_opt_values.py ---

#[test]
fn test_a_following_flag_is_not_the_value() {
    let a = args(&["--resume", "-m", "grok-4"]);
    assert_eq!(_grok_opt_value(&a, &["--resume"]), None);
    assert_eq!(_codex_opt_value(&a, &["--resume"]), None);
}

#[test]
fn test_a_trailing_bare_option_has_no_value() {
    let a = args(&["--resume"]);
    assert_eq!(_grok_opt_value(&a, &["--resume"]), None);
    assert_eq!(_codex_opt_value(&a, &["--resume"]), None);
}

#[test]
fn test_a_real_value_still_reads() {
    let a = args(&["--resume", "old-sid", "-m", "grok-4"]);
    assert_eq!(
        _grok_opt_value(&a, &["--resume"]),
        Some("old-sid".to_string())
    );
    assert_eq!(
        _codex_opt_value(&a, &["--resume"]),
        Some("old-sid".to_string())
    );
}

#[test]
fn test_the_equals_form_still_reads() {
    let a = args(&["--resume=old-sid"]);
    assert_eq!(
        _grok_opt_value(&a, &["--resume"]),
        Some("old-sid".to_string())
    );
    assert_eq!(
        _codex_opt_value(&a, &["--resume"]),
        Some("old-sid".to_string())
    );
}

#[test]
fn test_codex_cwd_does_not_swallow_the_next_flag() {
    assert_eq!(
        _codex_opt_value(&args(&["--cd", "--model", "x"]), &["--cd", "-C"]),
        None
    );
    assert_eq!(
        _codex_opt_value(&args(&["--cd", "/tmp/w", "--model", "x"]), &["--cd", "-C"]),
        Some("/tmp/w".to_string())
    );
}

#[test]
fn test_grok_resume_before_a_flag_leaves_the_pane_unrecorded() {
    // a bare --resume opens grok's picker: hive cannot know the session id,
    // so it records nothing rather than recording the next flag
    assert_eq!(
        _grok_launch_session(&args(&["--resume", "-m", "grok-4"])),
        (None, false)
    );
}

#[test]
fn test_grok_resume_with_an_id_records_that_session() {
    assert_eq!(
        _grok_launch_session(&args(&["--resume", "old-sid"])),
        (Some("old-sid".to_string()), false)
    );
}

#[test]
fn test_grok_bare_launch_mints_a_session_and_passes_the_flag() {
    let (sid, pass_flag) = _grok_launch_session(&args(&["-m", "grok-4"]));
    assert!(pass_flag);
    assert_eq!(sid.expect("minted session id").len(), 36);
}

// --- tests/unit/test_pr_window_display.py ---

#[test]
fn test_derives_plain_padded_format() {
    assert_eq!(
        _derive_pr_window_status(Some("  #I #W  ")),
        Some("  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  ".to_string())
    );
}

#[test]
fn test_preserves_style_wrappers_and_padding() {
    let derived = _derive_pr_window_status(Some("#[bg=yellow,fg=black,bold]  #I #W  #[default]"));
    assert_eq!(
        derived,
        Some(
            "#[bg=yellow,fg=black,bold]  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  #[default]"
                .to_string()
        )
    );
}

#[test]
fn test_derives_tmux_default_format() {
    let derived = _derive_pr_window_status(Some("#I:#W#{?window_flags,#{window_flags}, }"));
    assert_eq!(
        derived,
        Some("#{?#{@hive-pr},PR#{@hive-pr},#I}:#W#{?window_flags,#{window_flags}, }".to_string())
    );
}

#[test]
fn test_skips_when_global_already_references_hive_pr() {
    assert_eq!(
        _derive_pr_window_status(Some("#{?#{@hive-pr},PR#{@hive-pr},#I}:#W")),
        None
    );
}

#[test]
fn test_skips_when_no_index_token() {
    assert_eq!(_derive_pr_window_status(Some("#W only")), None);
}

#[test]
fn test_skips_empty_or_missing_global() {
    assert_eq!(_derive_pr_window_status(None), None);
    assert_eq!(_derive_pr_window_status(Some("")), None);
}

#[test]
fn test_escaped_literal_hash_i_is_not_rewritten() {
    // `##I` renders a literal `#I` — not a replaceable index token, so skip.
    assert_eq!(_derive_pr_window_status(Some("##I #W")), None);
}

#[test]
fn test_replaces_real_tokens_and_leaves_escaped_ones() {
    let derived = _derive_pr_window_status(Some("#I #W ##I #I"));
    assert_eq!(
        derived,
        Some(format!("{_PR_INDEX_TOKEN} #W ##I {_PR_INDEX_TOKEN}"))
    );
}

// --- tests/unit/test_launcher_mint_names.py ---

fn tags_lookup<'a>(
    mapping: &'a [((&'a str, &'a str), &'a str)],
) -> impl Fn(&str, &str) -> Option<String> + 'a {
    move |target: &str, key: &str| {
        mapping
            .iter()
            .find(|((t, k), _)| *t == target && *k == key)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn test_a_member_pane_mints_the_member_name_for_claude() {
    let mapping = [
        (("%179", "hive-team"), "honey"),
        (("%179", "hive-agent"), "worker"),
    ];
    let label = _pane_member_label_via(tags_lookup(&mapping), "%179");
    assert_eq!(_mint_name(label, "%179"), "honey.worker");
}

#[test]
fn test_a_member_pane_mints_the_member_name_for_codex() {
    let mapping = [
        (("%9", "hive-team"), "comb"),
        (("%9", "hive-agent"), "validator"),
    ];
    let label = _pane_member_label_via(tags_lookup(&mapping), "%9");
    assert_eq!(_mint_name(label, "%9"), "comb.validator");
}

#[test]
fn test_an_untagged_pane_falls_back_to_the_pane_placeholder() {
    let mapping: [((&str, &str), &str); 0] = [];
    let label = _pane_member_label_via(tags_lookup(&mapping), "%42");
    assert_eq!(_mint_name(label, "%42"), "hive-42");
}

#[test]
fn test_a_half_tagged_pane_is_not_a_member() {
    let mapping = [(("%7", "hive-team"), "honey")];
    let label = _pane_member_label_via(tags_lookup(&mapping), "%7");
    assert_eq!(_mint_name(label, "%7"), "hive-7");
}

// --- launcher scanning / resume parsing ---

#[test]
fn test_codex_subcommand_index_skips_global_options() {
    assert_eq!(
        _codex_subcommand_index(&args(&["-c", "k=v", "exec"])),
        Some(2)
    );
    assert_eq!(_codex_subcommand_index(&args(&["resume", "sid"])), Some(0));
    assert_eq!(_codex_subcommand_index(&args(&["-m", "gpt"])), None);
}

#[test]
fn test_codex_positional_after_skips_flags() {
    let a = args(&["resume", "--model", "x", "sid-1"]);
    assert_eq!(_codex_positional_after(&a, 0), Some("sid-1".to_string()));
    assert_eq!(_codex_positional_after(&args(&["resume"]), 0), None);
}

#[test]
fn test_claude_resume_arg_shapes() {
    assert_eq!(_claude_resume_arg(&args(&[])), (false, None));
    assert_eq!(_claude_resume_arg(&args(&["--resume"])), (true, None));
    assert_eq!(
        _claude_resume_arg(&args(&["-r", "abc"])),
        (true, Some("abc".to_string()))
    );
    assert_eq!(
        _claude_resume_arg(&args(&["--resume=abc"])),
        (true, Some("abc".to_string()))
    );
    assert_eq!(_claude_resume_arg(&args(&["--resume", "-m"])), (true, None));
    assert_eq!(_claude_resume_arg(&args(&["--resume="])), (true, None));
}

// --- fork split choice ---

#[test]
fn test_choose_fork_split_prefers_fitting_direction() {
    // Both fit: wide window goes horizontal only at >= 2.5x aspect.
    assert!(_choose_fork_split(300, 60));
    assert!(!_choose_fork_split(200, 100));
    // Only horizontal fits.
    assert!(_choose_fork_split(200, 30));
    // Only vertical fits.
    assert!(!_choose_fork_split(100, 60));
    // Neither fits: highest score wins (h_score 0.9875 vs v_score 0.45).
    assert!(_choose_fork_split(159, 20));
    assert!(!_choose_fork_split(80, 41));
}

// --- config value parsing ---

#[test]
fn test_parse_config_value_shapes() {
    assert_eq!(_parse_config_value("true"), Value::Bool(true));
    assert_eq!(_parse_config_value(" FALSE "), Value::Bool(false));
    assert_eq!(_parse_config_value("42"), json!(42));
    assert_eq!(_parse_config_value("1.5"), json!(1.5));
    assert_eq!(
        _parse_config_value("hello"),
        Value::String("hello".to_string())
    );
}

// --- python-style json dumps ---

#[test]
fn test_py_dumps_matches_python_separators() {
    let value = json!({"a": 1, "b": [1, 2], "c": "x"});
    assert_eq!(
        py_dumps(&value, true, None, false),
        r#"{"a": 1, "b": [1, 2], "c": "x"}"#
    );
    assert_eq!(
        py_dumps(&json!({"b": 1, "a": 2}), true, None, true),
        r#"{"a": 2, "b": 1}"#
    );
}

#[test]
fn test_py_dumps_indent_matches_python() {
    let value = json!({"a": [1], "b": {}});
    assert_eq!(
        py_dumps(&value, true, Some(2), false),
        "{\n  \"a\": [\n    1\n  ],\n  \"b\": {}\n}"
    );
}

#[test]
fn test_py_dumps_ensure_ascii_escapes_non_ascii() {
    assert_eq!(py_dumps(&json!("你"), true, None, false), "\"\\u4f60\"");
    assert_eq!(py_dumps(&json!("你"), false, None, false), "\"你\"");
    assert_eq!(
        py_dumps(&json!("🐝"), true, None, false),
        "\"\\ud83d\\udc1d\""
    );
}

// --- shlex quoting ---

#[test]
fn test_shlex_quote_matches_python() {
    assert_eq!(shlex_quote(""), "''");
    assert_eq!(shlex_quote("abc./_-"), "abc./_-");
    assert_eq!(shlex_quote("a b"), "'a b'");
    assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
}

#[test]
fn test_uuid4_shape() {
    let sid = uuid4();
    assert_eq!(sid.len(), 36);
    assert_eq!(sid.as_bytes()[14], b'4');
    assert!(matches!(sid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
}

#[test]
fn test_plugin_setup_drives_both_clis_in_order() {
    let mut env = EnvGuard::new();
    let tmp = tempfile::tempdir().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("calls.log");
    for cli in ["claude", "codex"] {
        let path = bin.join(cli);
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"{cli} $*\" >> {}\n", log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    env.set("PATH", format!("{}:/usr/bin:/bin", bin.display()));

    admin::plugin_setup();

    let mp = tmp.path().join(".hive/core_assets/marketplace");
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(
        calls,
        vec![
            format!(
                "claude plugin marketplace add {}",
                mp.join("claude").display()
            ),
            "claude plugin install hive@hive --yes".to_string(),
            "claude plugin update hive@hive --yes".to_string(),
            format!(
                "codex plugin marketplace add {}",
                mp.join("codex").display()
            ),
            "codex plugin add hive@hive".to_string(),
        ]
    );
}

// --- spawn --task preflight (the workspace gate runs before any side effect) ---

#[test]
fn test_task_dispatch_workspace_fails_before_the_spawn_when_none_resolves() {
    let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
    let tmp = tempfile::TempDir::new().unwrap();
    env.set("HIVE_HOME", tmp.path().join(".hive"));
    let workspaceless = crate::team::Team {
        name: "hornet".to_string(),
        ..Default::default()
    };

    // no --task: nothing is required, nothing is resolved
    assert_eq!(
        spawn::_task_dispatch_workspace(&workspaceless, None).unwrap(),
        None
    );

    let err = spawn::_task_dispatch_workspace(&workspaceless, Some("/tmp/task.md"))
        .expect_err("a task dispatch with no workspace must refuse");
    assert!(err.to_string().contains("workspace not found"), "{err}");

    let with_workspace = crate::team::Team {
        name: "hornet".to_string(),
        workspace: "/tmp/ws-hn".to_string(),
        ..Default::default()
    };
    assert_eq!(
        spawn::_task_dispatch_workspace(&with_workspace, Some("/tmp/task.md")).unwrap(),
        Some("/tmp/ws-hn".to_string())
    );
}

// --- handler-level tests: doctor / spawn --task / inject / capture -------
//
// Same shape as the attach/render tests above: a registry under a temp
// `$HIVE_HOME`, the tmux double recording argv, and the seams the handler
// crosses answered by their own hooks. The oracle is always something the
// handler left behind — a registry row, a recorded request, an argv.

/// Healthy identity for the hived hook's ping, so `_ensure_team_hived`
/// believes a hived is up and starts none. Every other request still goes
/// to the real workspace socket: unanswered when nothing listens there,
/// answered by `fake_hived` when a test binds it.
fn hived_answering_ping(team: &str) -> crate::hived::testhook::Guard {
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

/// A hived stand-in on the workspace socket: records every request it is
/// sent and answers each with `{ok: true, msgId: "m-<n>"}`.
struct FakeHived {
    path: std::path::PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Map<String, Value>>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeHived {
    fn bind(workspace: &str) -> FakeHived {
        use std::io::{Read, Write};
        let path = crate::hived::_socket_path(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop_seen, log) = (
            std::sync::Arc::clone(&stop),
            std::sync::Arc::clone(&requests),
        );
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_seen.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { break };
                let mut body = Vec::new();
                let _ = stream.read_to_end(&mut body);
                let request: Map<String, Value> = serde_json::from_slice(&body).unwrap();
                let mut log = log.lock().unwrap();
                log.push(request);
                let reply = json!({"ok": true, "msgId": format!("m-{}", log.len())});
                let _ = stream.write_all(reply.to_string().as_bytes());
            }
        });
        FakeHived {
            path,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Map<String, Value>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeHived {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn test_doctor_without_a_reachable_hived_reports_run_dir_and_logs() {
    let env = display_env();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    std::fs::create_dir_all(&ws).unwrap();
    let argv = fake_tmux("", &[]);
    let _hived = hived_answering_ping("honey");
    let mut t = Team {
        name: "honey".to_string(),
        workspace: ws.clone(),
        ..Default::default()
    };

    let (report, healthy) = core_cmds::_doctor_report(&mut t, &ws, "orch");

    assert!(!healthy);
    let workspace = std::path::Path::new(&ws);
    assert_eq!(report["workspace"], Value::from(ws.as_str()));
    assert_eq!(
        report["runDir"],
        Value::from(
            crate::devlog::run_dir(workspace)
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        report["logs"],
        Value::Object(crate::devlog::log_paths(workspace))
    );
    assert_eq!(report["hived"]["ok"], Value::Bool(false));
    assert_eq!(
        report["hived"]["error"],
        Value::from(crate::devlog::hived_unavailable_message(workspace))
    );
    assert!(report.get("duplicateTeams").is_none());
    // The hook answered the ping, so no hived was started, and the socket
    // the doctor request then looked for is still absent.
    assert!(!crate::hived::_socket_path(&ws).exists());
    // Read-only on tmux: window identity and duplicate-binding lookups.
    assert!(argv
        .borrow()
        .iter()
        .all(|a| matches!(a[0].as_str(), "display-message" | "list-windows")));
}

#[test]
fn test_spawn_with_a_task_rosters_the_member_on_a_pane_and_dispatches_the_artifact() {
    let env = display_env();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    std::fs::create_dir_all(&ws).unwrap();
    let task = env._tmp.path().join("task.md");
    std::fs::write(&task, "review the diff\n").unwrap();
    let task_path = std::fs::canonicalize(&task)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    crate::registry::record_team(
        "honey",
        &ws,
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@7",
    )
    .unwrap();
    // The team window is up (`Team::load` resolves it through team.rs's
    // fake), so no heal runs. The caller's own pane is orch's, which is who
    // signs the dispatch.
    let argv = fake_tmux_tagged(
        "dev:2\t@7\thoney\t\t\t\n",
        &[],
        &[("%0", "hive-team", "honey"), ("%0", "hive-agent", "orch")],
    );
    let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
    let _hived = hived_answering_ping("honey");
    let fake_hived = FakeHived::bind(&ws);

    spawn::spawn(
        "bee",
        "",
        "",
        "",
        "",
        &[],
        Some("claude"),
        Some(task.to_string_lossy().as_ref()),
        "honey",
    );

    // The member exists in the registry: claude, the spawner's cwd (no
    // --cwd was given). Its identity is the pane→job record the spawn
    // wrote — the roster row reads its sessionId back from that record,
    // which the agent seam captured here instead of writing.
    let entry = crate::registry::load("honey").unwrap();
    let bee = entry["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "bee")
        .expect("bee on the roster");
    assert_eq!(bee["cli"], Value::from("claude"));
    assert_eq!(bee["cwd"], Value::from(getcwd()));
    let records = crate::agent::testhook::with(|h| h.records.clone()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, "%0");
    assert_eq!(records[0].1, "abcd1234");
    // The engine was minted under the member's label with the bootstrap
    // prompt alone — the task rides the message, not the birth prompt.
    let spawns = crate::agent::testhook::with(|h| h.spawns.clone()).unwrap();
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    assert_eq!(spawns[0].name, "honey.bee");
    assert_eq!(
        spawns[0].prompt,
        crate::agent::compose_initial_prompt("claude", "hive:hive", "", "honey")
    );
    assert!(!spawns[0].prompt.contains(&task_path));
    // One send reached the hived: orch → bee, the artifact being the task
    // file by its canonical path.
    let requests = fake_hived.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let sent = &requests[0];
    assert_eq!(sent["action"], Value::from("send"));
    assert_eq!(sent["team"], Value::from("honey"));
    assert_eq!(sent["senderAgent"], Value::from("orch"));
    assert_eq!(sent["targetAgent"], Value::from("bee"));
    assert_eq!(sent["body"], Value::from("task dispatch: task.md"));
    assert_eq!(sent["artifact"], Value::from(task_path.as_str()));
    assert_eq!(sent["replyTo"], Value::from(""));
    // The window was there: no heal. The member's pane came from the agent
    // seam's split echo (it never reaches the tmux facade), tagged for the
    // team.
    assert_eq!(count(&argv, "new-window"), 0);
    let tags = crate::agent::testhook::with(|h| h.tags.clone()).unwrap();
    assert!(
        tags.iter()
            .any(|(_, role, agent, team)| role == "agent" && agent == "bee" && team == "honey"),
        "{tags:?}"
    );
}

#[test]
fn test_spawn_rebuilds_a_missing_window_before_splitting() {
    let env = display_env();
    let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
    std::fs::create_dir_all(&ws).unwrap();
    // `@3` is the dead window's id: the heal must record the new one.
    crate::registry::record_team(
        "honey",
        &ws,
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@3",
    )
    .unwrap();
    // No window claims the team: the display is gone (server restart,
    // window closed by hand).
    let argv = fake_tmux_tagged(
        "",
        &[],
        &[("%0", "hive-team", "honey"), ("%0", "hive-agent", "orch")],
    );
    let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
    let _hived = hived_answering_ping("honey");

    spawn::spawn("bee", "", "", "", "", &[], Some("claude"), None, "honey");

    // The heal rebuilt the window first, and the member landed on the
    // roster with the new window's id in the display cache.
    assert_eq!(count(&argv, "new-window"), 1);
    let entry = crate::registry::load("honey").unwrap();
    assert_eq!(entry["display"], Value::from("@7"));
    assert!(entry["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == "bee"));
    // The split anchored on the healed window's first pane (`%1`, the pane
    // `new-window` minted), not on the caller's own `%0`: the re-resolve
    // after the heal is what puts the member in the team window.
    let records = crate::agent::testhook::with(|h| h.records.clone()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, "%1");
}

// --- create outside tmux: the team session is built at create time -------

#[test]
fn test_create_outside_tmux_builds_the_team_session_and_records_the_display() {
    let env = display_env_outside();
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_create_detached_team("honey", "", "", false, &["k=v".to_string()]);

    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "honey",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    assert!(has_row(&argv, &["rename-window", "-t", "honey:1", "honey"]));
    assert_eq!(count(&argv, "new-window"), 0);
    let entry = crate::registry::load("honey").expect("team recorded");
    assert_eq!(entry["display"], Value::from("@7"));
    // The default workspace is the team's own directory under the registry
    // store, beside its team.json — no /tmp slug, no session name in it.
    let ws = team_dir(&env, "honey");
    assert_eq!(
        entry["workspace"],
        Value::from(ws.to_string_lossy().as_ref())
    );
    assert!(ws.join("team.json").is_file());
    assert_eq!(entry["members"], Value::Array(Vec::new()));
    // The first pane is the team's dock, tagged as a shell-pane create
    // tags its pane, so a verb run from it finds the team through its own
    // tags (the window's `@hive-team` is display, not binding).
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-team", "honey"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "terminal"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "orch"]
    ));
    // usable, not just recorded: the bus a dispatch rides exists
    assert!(ws.join("hive.db").is_file(), "{}", ws.display());
    assert!(ws.join("artifacts").is_dir(), "{}", ws.display());
    assert!(ws.join("run").is_dir(), "{}", ws.display());
    // --state lands on the default workspace too, not only an explicit one
    assert_eq!(
        std::fs::read_to_string(ws.join("state").join("k")).unwrap(),
        "v"
    );
}

fn team_dir(env: &DisplayEnv, team: &str) -> std::path::PathBuf {
    env._tmp.path().join(".hive").join("teams").join(team)
}

#[test]
fn test_create_outside_tmux_resets_a_recycled_names_leftover_workspace() {
    let env = display_env_outside();
    // `hive delete honey` kept the predecessor's workspace files
    let ws = team_dir(&env, "honey");
    std::fs::create_dir_all(ws.join("artifacts")).unwrap();
    std::fs::write(ws.join("artifacts").join("old.md"), "stale").unwrap();
    std::fs::write(ws.join("hive.db"), "stale").unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_create_detached_team("honey", "", "", false, &[]);

    assert!(crate::registry::load("honey").is_some());
    assert!(!ws.join("artifacts").join("old.md").exists());
    assert_ne!(std::fs::read(ws.join("hive.db")).unwrap(), b"stale");
}

#[test]
fn test_create_outside_tmux_honours_an_explicit_workspace_beside_the_team_dir() {
    let env = display_env_outside();
    let external = env._tmp.path().join("elsewhere");
    std::fs::create_dir_all(external.join("artifacts")).unwrap();
    std::fs::write(external.join("artifacts").join("keep.md"), "x").unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_create_detached_team("honey", "", external.to_str().unwrap(), false, &[]);

    let entry = crate::registry::load("honey").expect("team recorded");
    assert_eq!(
        entry["workspace"],
        Value::from(external.to_string_lossy().as_ref())
    );
    // the entry still lives in the team dir; the workspace elsewhere is
    // initialized, not wiped
    assert!(team_dir(&env, "honey").join("team.json").is_file());
    assert!(!team_dir(&env, "honey").join("hive.db").exists());
    assert!(external.join("hive.db").is_file());
    assert!(external.join("artifacts").join("keep.md").is_file());
}

#[test]
fn test_create_inside_tmux_from_a_shell_pane_defaults_to_the_team_dir() {
    let env = display_env();
    let _argv = fake_tmux_sessions("", &[], &[], &["dev"]);

    core_cmds::create("honey", "", "", false, &["k=v".to_string()]);

    let entry = crate::registry::load("honey").expect("team recorded");
    // the team dir, not /tmp/hive-<session>-w<id>
    let ws = team_dir(&env, "honey");
    assert_eq!(
        entry["workspace"],
        Value::from(ws.to_string_lossy().as_ref())
    );
    assert!(ws.join("team.json").is_file());
    assert!(ws.join("hive.db").is_file());
    assert_eq!(
        std::fs::read_to_string(ws.join("state").join("k")).unwrap(),
        "v"
    );
}

// --- flow rig: the run's team dir is its default workspace ----------------

#[test]
fn test_rig_up_defaults_the_workspace_to_the_team_dir_and_keeps_its_journal() {
    let env = display_env_outside();
    let ws = team_dir(&env, "abc");
    // a previous rig's journal, kept by `--down` for `--resume`
    std::fs::create_dir_all(ws.join("artifacts").join("flow")).unwrap();
    std::fs::write(ws.join("artifacts").join("flow").join("ops.jsonl"), "{}").unwrap();
    let _argv = fake_tmux_sessions("", &[], &[], &[]);

    assert_eq!(crate::flow_rig::rig_cmd("abc", None, None, false), 0);

    let entry = crate::registry::load("abc").expect("team recorded");
    assert_eq!(
        entry["workspace"],
        Value::from(ws.to_string_lossy().as_ref())
    );
    assert!(ws.join("team.json").is_file());
    assert!(ws.join("hive.db").is_file());
    assert!(ws
        .join("artifacts")
        .join("flow")
        .join("ops.jsonl")
        .is_file());
}

/// This process is a live Claude session `me` (sessionId `s-me`): its
/// inbox socket names its registration, whose sessionId is an interactive
/// one — no bg job row, so the mirror lane is `hive view`.
fn claude_session_me(env: &mut DisplayEnv) -> crate::adapters::claude_bg::testhook::Guard {
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

#[test]
fn test_create_outside_tmux_seats_a_claude_session_creator_as_orch_on_a_mirror_pane() {
    let mut env = display_env_outside();
    env.env.set("HIVE_BIN", "/x/hive");
    let _claude = claude_session_me(&mut env);
    let _hived = hived_answering_ping("honey");
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_create_detached_team("honey", "", "", false, &[]);

    let entry = crate::registry::load("honey").expect("team recorded");
    let mut orch = Map::new();
    orch.insert("name".to_string(), Value::from("orch"));
    orch.insert("cli".to_string(), Value::from("claude"));
    orch.insert("model".to_string(), Value::from(""));
    orch.insert("sessionId".to_string(), Value::from("s-me"));
    orch.insert("cwd".to_string(), Value::from(getcwd()));
    assert_eq!(entry["members"], Value::Array(vec![Value::Object(orch)]));
    // The first pane is the creator's read-only mirror: tagged as orch,
    // running `hive view` on the session — never a resume, which would mint
    // a forked job. No second pane.
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "orch"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
    assert!(argv.borrow().iter().any(|a| a[0] == "send-keys"
        && a.contains(&"-l".to_string())
        && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert_eq!(count(&argv, "split-window"), 0);
    // The mirror on screen is what makes the orch chip appear.
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "honey:1", "@hive-mirror", "on"]
    ));
    assert_status_bar_installed(&argv);
}

/// The team session's status bar, by session id, then the two bindings
/// naming this binary (`HIVE_BIN`).
fn assert_status_bar_installed(argv: &Argv) {
    for row in crate::tmux::team_status_argv("$1") {
        let row: Vec<&str> = row.iter().map(String::as_str).collect();
        assert!(has_row(argv, &row), "{row:?}");
    }
    // The double answers `list-keys` with nothing: no prefix+m fallback.
    for row in [
        crate::tmux::status_click_binding("/x/hive"),
        crate::tmux::mirror_key_binding("/x/hive", ""),
    ] {
        let row: Vec<&str> = row.iter().map(String::as_str).collect();
        assert!(has_row(argv, &row), "{row:?}");
    }
}

#[test]
fn test_create_outside_tmux_without_a_session_installs_the_bar_but_no_mirror_chip() {
    let mut env = display_env_outside();
    env.env.set("HIVE_BIN", "/x/hive");
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_create_detached_team("honey", "", "", false, &[]);

    assert_status_bar_installed(&argv);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "set-window-option" && a[3] == "@hive-mirror")));
}

#[test]
fn test_pane_role_draws_the_mirror_unless_the_preference_is_off() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    let argv = fake_tmux_tagged("dev:1\t@7\thoney\t\t\t\n", &[], &[]);
    let orch = member_row("orch", "claude", "s-me");

    assert_eq!(_pane_role(None, &orch), Some("mirror"));
    assert_eq!(_pane_role(Some(true), &orch), Some("mirror"));
    assert_eq!(_pane_role(Some(false), &orch), None);
    // An engine member never mirrors, whatever the window records.
    assert_eq!(
        _pane_role(Some(false), &member_row("sage", "grok", "sid")),
        Some("agent")
    );
    assert_eq!(count(&argv, "set-window-option"), 0);
}

#[test]
fn test_attach_heal_respects_hive_mirror_off() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_tagged(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:2", "hive-mirror", "off")],
    );

    attach_cmd("honey");

    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "send-keys"), 0);
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_keeps_the_mirror_the_window_already_shows() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
    );

    attach_cmd("honey");

    // The mirror counts as the member's pane: no second one, nothing moved.
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "send-keys"), 0);
    assert_eq!(count(&argv, "kill-pane"), 0);
    assert_eq!(count(&argv, "break-pane"), 0);
    assert!(argv.borrow().iter().all(|a| a[0] != "set-window-option"));
    assert!(has_row(&argv, &["switch-client", "-t", "dev:2"]));
}

#[test]
fn test_attach_heal_joins_the_hidden_mirror_instead_of_splitting() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    // The window records nothing; the orch's mirror is parked from an
    // earlier `hive mirror off` on a window since killed by hand.
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "orch"),
        ],
    );

    attach_cmd("honey");

    // The parked pane comes back — its viewer intact, never a second one —
    // without the notify mark a fire while parked left on it.
    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "-u", "@hive-notify-active"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
    assert_eq!(count(&argv, "select-layout"), 1);
    // A mirror on screen makes the orch chip appear.
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
}

#[test]
fn test_attach_heal_splits_a_fresh_viewer_when_the_parked_pane_is_another_members() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "scout"),
        ],
    );

    attach_cmd("honey");

    // scout's parked pane stays parked; the orch gets its own viewer.
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}

#[test]
fn test_attach_rebuild_hands_the_first_pane_to_the_next_member_when_the_mirror_is_withheld() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "claude", "s-me"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "",
    )
    .unwrap();
    let argv = fake_tmux_tagged("", &[], &[("dev:2", "hive-mirror", "off")]);

    attach_cmd("honey");

    // The withheld mirror consumes no pane: sage takes the window's own.
    assert_eq!(count(&argv, "split-window"), 0);
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "agent"]
    ));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-agent", "sage"]
    ));
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !(a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view")))));
}

#[test]
fn test_attach_heal_builds_the_mirror_when_not_suppressed() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux(_MIRROR_WINDOW, &["%0\t\tzsh\tterminal\t\thoney\t\t"]);

    attach_cmd("honey");

    assert_eq!(count(&argv, "split-window"), 1);
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    // An ordinary pane: the landscape preset, no generated layout.
    assert!(has_row(
        &argv,
        &["select-layout", "-t", "dev:1", "main-vertical"]
    ));
}

// --- hive mirror -------------------------------------------------------------

const _MIRROR_WINDOW: &str = "dev:1\t@7\thoney\t\t\t\n";

fn honey_with_a_session_orch() {
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "claude", "s-me")],
        "@7",
    )
    .unwrap();
}

const _BREAK_PANE_TAIL: [&str; 5] = [
    "-n",
    "honey·mirror",
    "-P",
    "-F",
    "#{session_name}:#{window_index}\t#{pane_id}",
];

#[test]
fn test_mirror_off_breaks_the_pane_into_the_team_session_records_off_and_retiles() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_sessions(
        _MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\torch\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
            "%2\t[sage]\tgrok\tagent\tsage\thoney\tgrok\t",
        ],
        &[("dev:1", "hive-team", "honey")],
        &["dev", "honey"],
    );

    assert_eq!(_mirror("off", ""), Ok("mirror off (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
    let mut row = vec!["break-pane", "-s", "%1", "-d", "-t", "=honey:"];
    row.extend(&_BREAK_PANE_TAIL);
    assert!(has_row(&argv, &row));
    assert!(has_row(
        &argv,
        &[
            "set-window-option",
            "-t",
            "honey:9",
            "@hive-hidden",
            "honey"
        ]
    ));
    assert_eq!(count(&argv, "kill-pane"), 0);
    // The two survivors get the landscape preset.
    assert!(has_row(
        &argv,
        &["select-layout", "-t", "dev:1", "main-vertical"]
    ));
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_mirror_off_without_a_team_session_parks_the_pane_in_the_callers_session() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\torch\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(_mirror("off", ""), Ok("mirror off (honey)".to_string()));

    let mut row = vec!["break-pane", "-s", "%1", "-d"];
    row.extend(&_BREAK_PANE_TAIL);
    assert!(has_row(&argv, &row));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:9", "@hive-hidden", "honey"]
    ));
}

#[test]
fn test_mirror_off_refuses_when_the_mirror_is_the_only_pane() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    let err = _mirror("off", "").unwrap_err();

    assert!(err.contains("only pane"), "{err}");
    // A refusal records nothing: the mirror is still on screen.
    assert_eq!(count(&argv, "set-window-option"), 0);
    assert_eq!(count(&argv, "break-pane"), 0);
}

#[test]
fn test_mirror_off_without_a_mirror_records_off_and_leaves_the_window_alone() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\torch\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        _mirror("off", ""),
        Ok("mirror off (honey): no mirror".to_string())
    );

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "off"]
    ));
    assert_eq!(count(&argv, "break-pane"), 0);
    assert_eq!(count(&argv, "select-layout"), 0);
}

#[test]
fn test_mirror_off_refuses_from_the_mirror_pane_but_not_with_window() {
    let mut env = display_env();
    env.env.set("TMUX_PANE", "%1");
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    let err = _mirror("off", "").unwrap_err();
    assert!(err.contains("mirror"), "{err}");
    assert_eq!(count(&argv, "break-pane"), 0);
    assert!(argv.borrow().iter().all(|a| a[0] != "set-window-option"));

    // The bindings name the window; a click is never "from" a pane.
    assert_eq!(
        _mirror("off", "dev:1"),
        Ok("mirror off (honey)".to_string())
    );
    assert_eq!(count(&argv, "break-pane"), 1);
}

#[test]
fn test_mirror_on_joins_the_hidden_pane_first_and_retiles() {
    let _env = display_env();
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "orch"),
        ],
    );

    assert_eq!(_mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_mirror_on_with_the_mirror_shown_says_so_and_leaves_the_window_alone() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        _mirror("on", ""),
        Ok("mirror on (honey): already shown".to_string())
    );

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 0);
}

#[test]
fn test_mirror_on_rebuilds_the_mirror_when_no_hidden_pane_exists() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
        ],
    );

    assert_eq!(_mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%1", "@hive-role", "mirror"]
    ));
}

#[test]
fn test_mirror_on_with_nothing_to_show_says_so() {
    let _env = display_env();
    // The flow-rig shape: a team whose roster has no session member and
    // whose rig mirror is gone for good.
    crate::registry::record_team("honey", "", "100.0", &[], "@7").unwrap();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    assert_eq!(
        _mirror("on", ""),
        Ok("mirror on (honey): no session mirror to show".to_string())
    );

    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 0);
    // Nothing shown, nothing recorded: no orch chip that toggles nothing.
    assert_eq!(count(&argv, "set-window-option"), 0);
}

#[test]
fn test_mirror_on_joins_a_parked_rig_mirror_that_names_no_member() {
    let _env = display_env();
    crate::registry::record_team("honey", "", "100.0", &[], "@7").unwrap();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
        ],
    );

    assert_eq!(_mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert!(has_row(
        &argv,
        &["join-pane", "-h", "-b", "-d", "-s", "%1", "-t", "%0"]
    ));
    assert!(has_row(
        &argv,
        &["set-window-option", "-t", "dev:1", "@hive-mirror", "on"]
    ));
    assert_eq!(count(&argv, "split-window"), 0);
    assert_eq!(count(&argv, "select-layout"), 1);
}

#[test]
fn test_mirror_on_leaves_another_members_parked_pane_alone() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    // scout's parked mirror is scout's: the orch gets a fresh viewer.
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[
            ("dev:1", "hive-team", "honey"),
            ("dev:1", "hive-mirror", "off"),
            ("%1", "hive-hidden", "honey"),
            ("%1", "hive-role", "mirror"),
            ("%1", "hive-agent", "scout"),
        ],
    );

    assert_eq!(_mirror("on", ""), Ok("mirror on (honey)".to_string()));

    assert_eq!(count(&argv, "join-pane"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}

#[test]
fn test_mirror_toggles_by_presence() {
    let mut env = display_env();
    let _claude = claude_session_me(&mut env);
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &["%0\t\tzsh\tterminal\t\thoney\t\t"],
        &[("dev:1", "hive-team", "honey")],
    );

    // No mirror: the toggle shows one…
    assert_eq!(_mirror("", ""), Ok("mirror on (honey)".to_string()));
    assert_eq!(count(&argv, "split-window"), 1);
    // …and with the mirror on screen the next toggle parks it.
    assert_eq!(_mirror("", ""), Ok("mirror off (honey)".to_string()));
    assert_eq!(count(&argv, "break-pane"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "break-pane" && a[2] == "%1"));
}

#[test]
fn test_mirror_window_flag_names_the_window() {
    // A run-shell job (the status click, prefix+m): TMUX but no TMUX_PANE.
    let mut env = display_env_outside();
    env.env.set("TMUX", "/tmp/hive-test-tmux,1,0");
    honey_with_a_session_orch();
    let argv = fake_tmux_tagged(
        _MIRROR_WINDOW,
        &[
            "%0\t\tzsh\tterminal\t\thoney\t\t",
            "%1\t[orch]\thive\tmirror\torch\thoney\tclaude\t",
        ],
        &[("dev:1", "hive-team", "honey")],
    );

    assert!(_mirror("on", "").is_err());
    assert_eq!(
        _mirror("on", "dev:1"),
        Ok("mirror on (honey): already shown".to_string())
    );
    assert_eq!(
        _mirror("off", "dev:1"),
        Ok("mirror off (honey)".to_string())
    );
    assert_eq!(count(&argv, "break-pane"), 1);
}

#[test]
fn test_mirror_outside_a_team_window_fails() {
    let _env = display_env();
    let _argv = fake_tmux("dev:1\t@7\t\t\t\t\n", &[]);

    let err = _mirror("on", "").unwrap_err();

    assert!(err.contains("hive ls"), "{err}");
}

// --- join outside tmux: the joined session gets its mirror pane now ------

fn joined_session_row(team: &str) -> Map<String, Value> {
    crate::registry::load(team).unwrap()["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sessionId"] == "s-me")
        .and_then(Value::as_object)
        .cloned()
        .expect("the joined session is on the roster")
}

#[test]
fn test_join_outside_tmux_adds_the_sessions_mirror_pane_to_the_team_window() {
    let mut env = display_env_outside();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "@7",
    )
    .unwrap();
    let argv = fake_tmux_sessions(
        "honey:1	@7	honey			
",
        &["%1	[orch]	grok	agent	orch	honey	grok	"],
        &[],
        &["honey"],
    );

    core_cmds::_join_as_ccd("honey", "");

    let joined = joined_session_row("honey");
    assert_eq!(joined["cli"], Value::from("claude"));
    assert_ne!(joined["name"], Value::from("orch"));
    // One pane split into the existing window, running the session's
    // read-only mirror — never a resume, which would fork a bg job — and
    // tagged as the window's mirror.
    assert_eq!(count(&argv, "new-window"), 0);
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv.borrow().iter().any(|a| a[0] == "send-keys"
        && a.contains(&"-l".to_string())
        && a.iter().any(|arg| arg.contains("hive view s-me"))));
    assert!(has_row(
        &argv,
        &["set-option", "-p", "-t", "%2", "@hive-role", "mirror"]
    ));
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "set-window-option" && a[3] == "@hive-mirror" && a[4] == "on"));
}

#[test]
fn test_join_outside_tmux_rebuilds_a_missing_team_window_first() {
    let mut env = display_env_outside();
    let _claude = claude_session_me(&mut env);
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[member_row("orch", "grok", "sid-orch")],
        "",
    )
    .unwrap();
    let argv = fake_tmux_sessions("", &[], &[], &[]);

    core_cmds::_join_as_ccd("honey", "");

    joined_session_row("honey");
    assert!(has_row(
        &argv,
        &[
            "new-session",
            "-d",
            "-s",
            "honey",
            "-x",
            "220",
            "-y",
            "60",
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ));
    // orch rides the first pane; the joined session gets the split.
    assert_eq!(count(&argv, "split-window"), 1);
    assert!(argv
        .borrow()
        .iter()
        .any(|a| a[0] == "send-keys" && a.iter().any(|arg| arg.contains("hive view s-me"))));
}

/// A rendered team as `Team::load` resolves it: orch and sage (grok) and
/// bee (claude) each on a pane of the team window. Built in memory — the
/// registry-plus-window resolution is `team.rs`'s own contract.
fn rendered_team() -> Team {
    use crate::agent::testhook::fake_agent;
    Team {
        name: "honey".to_string(),
        agents: vec![
            fake_agent("orch", "honey", "%1", "grok"),
            fake_agent("sage", "honey", "%2", "grok"),
            fake_agent("bee", "honey", "%3", "claude"),
        ],
        ..Default::default()
    }
}

#[test]
fn test_inject_types_into_the_members_pane_and_refuses_a_pane_with_no_composer() {
    let _env = display_env();
    let t = rendered_team();
    let _agent = crate::agent::testhook::install(crate::agent::testhook::Hook::new());
    let calls = || crate::agent::testhook::with(|h| std::mem::take(&mut h.calls)).unwrap();

    // A grok member: the text and Enter go to that member's pane.
    crate::agent::testhook::with(|h| h.resolve_profile_name = Some("grok".to_string()));
    let report = _inject_report(&t, "sage", "hello sage").unwrap();
    assert_eq!(report["pane"], Value::from("%2"));
    assert_eq!(report["member"], Value::from("sage"));
    assert_eq!(report["success"], Value::Bool(true));
    assert_eq!(calls(), vec!["hello sage", "<Enter>"]);

    // A claude member whose pane has no job record and no interactive
    // claude process (an attach viewer): refused by pane id, nothing typed.
    crate::agent::testhook::with(|h| {
        h.resolve_profile_name = Some("claude".to_string());
        h.job_id_for_pane = None;
        h.interactive_claude_pid = None;
    });
    let err = _inject_report(&t, "bee", "hello bee")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no interactive claude process on pane %3"),
        "{err}"
    );
    assert_eq!(calls(), Vec::<String>::new());

    // Not on the roster: named, and no pane is touched.
    let err = _inject_report(&t, "ghost", "hello")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("member 'ghost' not found in team 'honey'"),
        "{err}"
    );
    assert_eq!(calls(), Vec::<String>::new());
}

#[test]
fn test_capture_reads_the_members_own_pane() {
    let _env = display_env();
    let argv = fake_tmux("", &[]);
    let t = rendered_team();

    let text = _capture_text(&t, "sage", 40).unwrap();

    // One capture, of sage's pane — not the caller's (%0) nor orch's.
    assert_eq!(
        argv.borrow().as_slice(),
        &[args(&["capture-pane", "-t", "%2", "-p", "-S", "-40"])]
    );
    assert_eq!(text, "");
    let err = _capture_text(&t, "ghost", 40).unwrap_err().to_string();
    assert!(
        err.contains("member 'ghost' not found in team 'honey'"),
        "{err}"
    );
    assert_eq!(argv.borrow().len(), 1);
}

// --- shell-init / resume-hint ----------------------------------------------

/// The launcher script must be sourceable by both shells it claims and leave
/// the three launchers defined as functions.
#[test]
fn test_shell_init_script_parses_in_zsh_and_bash_and_defines_the_launchers() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hive-init.sh");
    std::fs::write(&path, _shell_init_script("zsh")).unwrap();
    let quoted = shlex_quote(&path.to_string_lossy());
    let run = |shell: &str, argv: &[&str]| {
        let out = std::process::Command::new(shell)
            .args(argv)
            .output()
            .unwrap_or_else(|e| panic!("{shell} must be runnable for this test: {e}"));
        assert!(
            out.status.success(),
            "{shell} {argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    // Syntax only, no rc files.
    run("zsh", &["-f", "-n", &path.to_string_lossy()]);
    run(
        "bash",
        &["--noprofile", "--norc", "-n", &path.to_string_lossy()],
    );
    // Sourced: each launcher is a function in both dialects.
    assert_eq!(
        run(
            "zsh",
            &[
                "-f",
                "-c",
                &format!("source {quoted}; whence -w hclaude hcodex hgrok")
            ]
        ),
        "hclaude: function\nhcodex: function\nhgrok: function\n"
    );
    assert_eq!(
        run(
            "bash",
            &[
                "--noprofile",
                "--norc",
                "-c",
                &format!("source {quoted}; declare -F hclaude hcodex hgrok"),
            ]
        ),
        "hclaude\nhcodex\nhgrok\n"
    );
}

#[test]
fn test_shell_init_resolves_the_dialect_from_shell_env() {
    let mut env = EnvGuard::new();
    assert_ne!(_shell_init_script("fish"), _shell_init_script("zsh"));
    env.set("SHELL", "/opt/homebrew/bin/fish");
    assert_eq!(_shell_init_script(""), _shell_init_script("fish"));
    env.set("SHELL", "/bin/bash");
    assert_eq!(_shell_init_script(""), _shell_init_script("zsh"));
    env.remove("SHELL");
    assert_eq!(_shell_init_script(""), _shell_init_script("zsh"));
}

#[test]
fn test_resume_hint_needs_a_tagged_member_pane_and_its_job_record() {
    let mut env = display_env();
    env.env.set("TMUX_PANE", "%5");
    let _argv = fake_tmux_tagged(
        "",
        &[],
        &[("%5", "hive-team", "honey"), ("%5", "hive-agent", "bee")],
    );

    // A member pane with no job record: nothing to resume, no hint.
    assert_eq!(_resume_hint("claude", "/tmp/w"), None);

    crate::adapters::claude_bg::write_pane_job("%5", "job-77", "sess-77", "/tmp/w").unwrap();
    let hint = _resume_hint("claude", "/tmp/w").expect("a recorded job is resumable");
    assert!(hint.starts_with("Resume from anywhere:\n  "), "{hint}");
    assert!(
        hint.contains("cd /tmp/w && hive claude --resume job-77"),
        "{hint}"
    );

    // The record alone is not enough: an untagged pane is nobody's member.
    env.env.set("TMUX_PANE", "%6");
    crate::adapters::claude_bg::write_pane_job("%6", "job-78", "sess-78", "/tmp/w").unwrap();
    assert_eq!(_resume_hint("claude", "/tmp/w"), None);
}
