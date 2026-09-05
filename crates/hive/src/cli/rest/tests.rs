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

// --- attach (jump only) / render (build, backfill, then jump) ---

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

fn member_row(name: &str, cli: &str, session_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("name".to_string(), Value::from(name));
    m.insert("cli".to_string(), Value::from(cli));
    m.insert("sessionId".to_string(), Value::from(session_id));
    m.insert("cwd".to_string(), Value::from("/tmp"));
    m
}

type Argv = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

/// A tmux double that answers everything `attach`/`render` ask and records
/// every argv. `windows` is the `list-windows -a` stdout; `panes` the pane
/// rows the team window already has (`_PANE_BASE_FMT` order, tab-joined).
///
/// Two seams, because `_find_team_window` resolves tmux through `team.rs`'s
/// own fake in test builds while `attach.rs` calls the real module. Only the
/// real module's argv is recorded — which is the half that writes.
fn fake_tmux(windows: &'static str, panes: &'static [&'static str]) -> Argv {
    fake_tmux_tagged(windows, panes, &[])
}

/// `fake_tmux` whose `show-options -p` also answers pane tags:
/// `(pane, key, value)` rows, `key` without the `@` (`hive-team`). Tagging
/// the caller's own pane (`%0` under `display_env`) is what lets a verb with
/// no team argument discover its team through the binding ladder.
fn fake_tmux_tagged(
    windows: &'static str,
    panes: &'static [&'static str],
    pane_tags: &'static [(&'static str, &'static str, &'static str)],
) -> Argv {
    crate::team::_set_fake_tmux_run(move |_args, _check| {
        Ok(crate::tmux::Run {
            returncode: 0,
            stdout: windows.to_string(),
            stderr: String::new(),
        })
    });
    let argv: Argv = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = std::rc::Rc::clone(&argv);
    let mut live: Vec<String> = panes
        .iter()
        .map(|row| row.split('\t').next().unwrap_or_default().to_string())
        .collect();
    let mut extra_rows: Vec<String> = Vec::new();
    let mut next_pane = 1;
    crate::tmux::_set_run_override(move |args, _check, _timeout| {
        recorded.borrow_mut().push(args.to_vec());
        let out = match args[0].as_str() {
            "list-windows" => windows.to_string(),
            "has-session" => String::new(),
            "new-window" => {
                let pane = format!("%{next_pane}");
                next_pane += 1;
                live.push(pane.clone());
                format!("dev:2\t{pane}")
            }
            "split-window" => {
                let pane = format!("%{next_pane}");
                next_pane += 1;
                live.push(pane.clone());
                extra_rows.push(format!("{pane}\t\t\t\t\t\t\t"));
                pane
            }
            "list-panes" => {
                let fmt = args.last().cloned().unwrap_or_default();
                if fmt == "#{pane_id}" {
                    live.join("\n")
                } else {
                    let mut rows: Vec<String> = panes.iter().map(|r| (*r).to_string()).collect();
                    rows.extend(extra_rows.iter().cloned());
                    rows.join("\n")
                }
            }
            "show-options" => {
                // show-options -p -v -t <pane> @<key>
                let pane = args.get(4).map(String::as_str).unwrap_or_default();
                let key = args
                    .get(5)
                    .and_then(|opt| opt.strip_prefix('@'))
                    .unwrap_or_default();
                pane_tags
                    .iter()
                    .find(|(p, k, _)| *p == pane && *k == key)
                    .map(|(_, _, v)| (*v).to_string())
                    .unwrap_or_default()
            }
            "display-message" => match args.last().map(String::as_str).unwrap_or_default() {
                "#{session_name}" => "dev".to_string(),
                "#{window_id}" => "@7".to_string(),
                "#{window_width}\t#{window_height}" => "200\t50".to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        Ok(crate::tmux::Run {
            returncode: 0,
            stdout: format!("{out}\n"),
            stderr: String::new(),
        })
    });
    argv
}

fn verbs(argv: &Argv) -> Vec<String> {
    argv.borrow().iter().map(|a| a[0].clone()).collect()
}

#[test]
fn test_attach_without_a_window_refuses_and_writes_nothing() {
    let _env = display_env();
    crate::registry::record_team(
        "honey",
        "",
        "100.0",
        &[
            member_row("orch", "grok", "sid-orch"),
            member_row("sage", "grok", "sid-sage"),
        ],
        "@3",
    )
    .unwrap();
    let argv = fake_tmux("", &[]);

    let message = _attach_target("honey").unwrap_err();

    // The team is alive; the message says so and names the verb that builds.
    assert!(message.contains("hive render honey"), "{message}");
    assert!(message.contains("2 member(s)"), "{message}");
    assert!(message.contains("@3"), "{message}");
    // Read-only: not one tmux command beyond the window lookup.
    assert_eq!(verbs(&argv), Vec::<String>::new());
    // And the registry's display cache is untouched.
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@3")
    );
}

#[test]
fn test_attach_names_the_missing_team_before_looking_at_tmux() {
    let _env = display_env();
    let argv = fake_tmux("", &[]);

    let message = _attach_target("ghost").unwrap_err();

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
    let argv = fake_tmux("dev:2\t@7\thoney\t\t\t\n", &[]);

    attach_cmd("honey");

    let recorded = argv.borrow();
    // switch-client moves *this* client; select-window would only retarget
    // the window's own session and leave the caller where it was.
    assert!(recorded
        .iter()
        .any(|a| a[..] == ["switch-client", "-t", "dev:2"]));
    assert!(recorded.iter().all(|a| a[0] != "select-window"));
    // Still read-only.
    assert!(recorded
        .iter()
        .all(|a| !matches!(a[0].as_str(), "new-window" | "split-window" | "send-keys")));
}

#[test]
fn test_render_without_a_window_builds_one_and_records_the_display() {
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

    render_cmd("honey");

    let recorded = argv.borrow();
    let verbs: Vec<&str> = recorded.iter().map(|a| a[0].as_str()).collect();
    // One window, and one split for the second attachable member; the
    // member with no engine identity gets no pane.
    assert_eq!(verbs.iter().filter(|v| **v == "new-window").count(), 1);
    assert_eq!(verbs.iter().filter(|v| **v == "split-window").count(), 1);
    assert!(recorded
        .iter()
        .any(|a| a[..] == ["switch-client", "-t", "dev:2"]));
    // The freshly built window id lands in the registry's display cache.
    assert_eq!(
        crate::registry::load("honey").unwrap()["display"],
        Value::from("@7")
    );
}

#[test]
fn test_render_with_a_window_backfills_without_a_second_window() {
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
    // The window renders `orch` only — `sage` was spawned after it was built.
    let argv = fake_tmux(
        "dev:2\t@7\thoney\t\t\t\n",
        &["%1\t[orch]\tgrok\tagent\torch\thoney\tgrok\t"],
    );

    render_cmd("honey");

    let recorded = argv.borrow();
    let verbs: Vec<&str> = recorded.iter().map(|a| a[0].as_str()).collect();
    assert!(!verbs.contains(&"new-window"));
    assert_eq!(verbs.iter().filter(|v| **v == "split-window").count(), 1);
    // The new pane runs sage's own viewer, not orch's.
    assert!(recorded
        .iter()
        .any(|a| a[0] == "send-keys" && a.last().is_some_and(|text| text.contains("sid-sage"))));
    assert!(recorded
        .iter()
        .any(|a| a[..] == ["switch-client", "-t", "dev:2"]));
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
    let mut hook = crate::hived::testhook::Hook::default();
    hook.request_ping = Some(std::sync::Arc::new(move |_ws: &str| {
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
    }));
    crate::hived::testhook::install(hook)
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
fn test_spawn_with_a_task_rosters_the_headless_member_and_dispatches_the_artifact() {
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
        "",
    )
    .unwrap();
    // No window for the team: the spawn is engine-only. The caller's own
    // pane is orch's, which is who signs the dispatch.
    let argv = fake_tmux_tagged(
        "",
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

    // The member exists in the registry: claude, the minted job as its
    // identity, the spawner's cwd (no --cwd was given).
    let entry = crate::registry::load("honey").unwrap();
    let bee = entry["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "bee")
        .expect("bee on the roster");
    assert_eq!(bee["cli"], Value::from("claude"));
    assert_eq!(bee["sessionId"], Value::from("abcd1234"));
    assert_eq!(bee["cwd"], Value::from(getcwd()));
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
    // Headless: tmux saw no window built and no keystroke.
    assert!(argv
        .borrow()
        .iter()
        .all(|a| !matches!(a[0].as_str(), "new-window" | "split-window" | "send-keys")));
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
