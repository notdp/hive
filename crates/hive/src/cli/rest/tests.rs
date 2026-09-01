// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

use serde_json::{json, Map, Value};

use super::*;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_attach_backfills_only_missing_attachable_members() {
    let member = |name: &str, cli: &str, sid: &str| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("name".to_string(), Value::from(name));
        m.insert("cli".to_string(), Value::from(cli));
        m.insert("sessionId".to_string(), Value::from(sid));
        m
    };
    let rendered: std::collections::HashSet<String> =
        ["orch".to_string(), "scout".to_string()].into();
    let picked = _members_to_backfill(
        &rendered,
        vec![
            member("orch", "claude", "sid-1"),  // already rendered
            member("scout", "claude", "sid-2"), // already rendered
            member("sage", "grok", "sid-3"),    // missing -> backfill
            member("ghost", "grok", ""),        // no engine identity
            member("shelly", "bash", "sid-4"),  // not an agent CLI
        ],
    );
    let names: Vec<String> = picked.iter().map(|m| map_str(m, "name")).collect();
    assert_eq!(names, vec!["sage".to_string()]);
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
