use std::path::Path;

use serde_json::{json, Map, Value};

use super::*;
use crate::tmux;

// ---------------------------------------------------------------------------
// pr
// ---------------------------------------------------------------------------

// Replaces the bare index token in a window-status format with a conditional
// that renders `PR<n>` for windows carrying `@hive-pr`. `##I` is tmux's
// escaped literal `#I`, not the index token — left alone (the pathological
// `###I` triple is intentionally unsupported: a conservative no-replace beats
// corrupting a user's format).
pub(crate) const PR_INDEX_TOKEN: &str = "#{?#{@hive-pr},PR#{@hive-pr},#I}";

fn replace_index_tokens(format: &str) -> String {
    let bytes = format.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'I'
            && (i == 0 || bytes[i - 1] != b'#')
        {
            out.extend_from_slice(PR_INDEX_TOKEN.as_bytes());
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| format.to_string())
}

/// Per-window status format derived from the *global* value; None = skip.
pub(crate) fn derive_pr_window_status(global_format: Option<&str>) -> Option<String> {
    let global_format = global_format?;
    if global_format.is_empty() {
        return None;
    }
    if global_format.contains("@hive-pr") {
        return None;
    }
    let derived = replace_index_tokens(global_format);
    if derived == global_format {
        return None; // no replaceable #I
    }
    Some(derived)
}

pub fn pr_set_cmd(number: i64, plain: bool) {
    if !tmux::is_inside_tmux() {
        fail("must run inside tmux");
    }
    if number <= 0 {
        fail(&format!(
            "PR number must be a positive integer, got {number}"
        ));
    }
    let window = tmux::get_current_window_target().unwrap_or_default();
    if window.is_empty() {
        fail("cannot determine current window");
    }
    if tmux::get_window_option(&window, "hive-team")
        .filter(|team| !team.is_empty())
        .is_none()
    {
        fail(
            "current window is not a hive team window (no @hive-team); \
             run `hive pr set` from your team window",
        );
    }
    tmux::set_window_option(&window, "@hive-pr", &number.to_string());
    let mut display = Map::new();
    for option in ["window-status-format", "window-status-current-format"] {
        let global_format = tmux::get_global_window_option(option);
        match derive_pr_window_status(global_format.as_deref()) {
            None => {
                let already = global_format
                    .as_deref()
                    .map(|f| !f.is_empty() && f.contains("@hive-pr"))
                    .unwrap_or(false);
                display.insert(
                    option.to_string(),
                    Value::String(
                        if already {
                            "already-global"
                        } else {
                            "skipped-no-index-token"
                        }
                        .to_string(),
                    ),
                );
            }
            Some(derived) => {
                tmux::set_window_option(&window, option, &derived);
                display.insert(option.to_string(), Value::String("derived".to_string()));
            }
        }
    }
    if plain {
        let summary = display
            .iter()
            .map(|(key, value)| match value {
                Value::String(s) => format!("{key}={s}"),
                other => format!("{key}={other}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("window {window} labeled @hive-pr={number} ({summary})");
    } else {
        let result = json!({"window": window, "pr": number, "display": display});
        println!("{}", json_pretty(&result));
    }
}

pub fn pr_clear_cmd(plain: bool) {
    if !tmux::is_inside_tmux() {
        fail("must run inside tmux");
    }
    let window = tmux::get_current_window_target().unwrap_or_default();
    if window.is_empty() {
        fail("cannot determine current window");
    }
    if tmux::get_window_option(&window, "hive-team")
        .filter(|team| !team.is_empty())
        .is_none()
    {
        fail(
            "current window is not a hive team window (no @hive-team); \
             run `hive pr clear` from your team window",
        );
    }
    let previous = tmux::get_window_option(&window, "hive-pr");
    tmux::clear_window_option(&window, "@hive-pr");
    if !plain {
        let previous_value = match &previous {
            Some(previous) => Value::String(previous.clone()),
            None => Value::Null,
        };
        println!(
            "{}",
            json_pretty(&json!({"window": window, "previous": previous_value}))
        );
    } else if previous.as_deref().is_some_and(|p| !p.is_empty()) {
        println!(
            "window {window} cleared @hive-pr={}",
            previous.unwrap_or_default()
        );
    } else {
        println!("window {window} had no @hive-pr stamp to clear");
    }
}

// ---------------------------------------------------------------------------
// worktree pool
// ---------------------------------------------------------------------------

fn wt_ok<T>(result: crate::worktree::Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(e) => fail(&e.to_string()),
    }
}

/// Owner / integration context for worktree commands (pane-anchored, cwd-free).
/// Returns (owner, team, integration).
fn worktree_context() -> (String, String, Option<String>) {
    let binding = discover_tmux_binding();
    let window = {
        let bound = map_str(&binding, "tmuxWindow");
        if !bound.is_empty() {
            bound
        } else if tmux::is_inside_tmux() {
            tmux::get_current_window_target().unwrap_or_default()
        } else {
            String::new()
        }
    };
    let team = map_str(&binding, "team");
    let integration = if window.is_empty() {
        None
    } else {
        tmux::get_window_option(&window, "hive-integration-branch").filter(|v| !v.is_empty())
    };
    let owner = if team.is_empty() {
        "unbound".to_string()
    } else {
        format!("team:{team}")
    };
    (owner, team, integration)
}

pub fn worktree_set_base_cmd(refname: &str, plain: bool) {
    let window = tmux::get_current_window_target().unwrap_or_default();
    let team = if window.is_empty() {
        String::new()
    } else {
        tmux::get_window_option(&window, "hive-team").unwrap_or_default()
    };
    if team.is_empty() {
        fail("current window is not a hive team window (no @hive-team); run from your team window");
    }
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let oid = wt_ok(crate::worktree::rev_parse(&anchor, refname));
    tmux::set_window_option(&window, "@hive-integration-branch", refname);
    if plain {
        println!(
            "team '{team}' integration branch set: {refname} ({})",
            &oid[..oid.len().min(12)]
        );
    } else {
        println!(
            "{}",
            json_pretty(&json!({
                "team": team,
                "integrationBranch": refname,
                "oid": oid,
                "window": window,
            }))
        );
    }
}

pub fn worktree_start_cmd(feature: &str, base_ref: Option<&str>, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let (owner, team, integration) = worktree_context();
    let base = wt_ok(crate::worktree::resolve_base(
        &anchor,
        base_ref,
        integration.as_deref(),
    ));
    let result = wt_ok(crate::worktree::start(
        &anchor,
        feature,
        &base,
        &owner,
        &team,
        integration.as_deref(),
        None,
    ));
    if plain {
        println!("{}", result.path);
        println!(
            "mode={} branch={} base={}@{}",
            result.mode,
            result.branch,
            result.base,
            &result.base_oid[..result.base_oid.len().min(12)]
        );
        for warning in &result.warnings {
            eprintln!("warning: {warning}");
        }
    } else {
        println!("{}", json_pretty(&result.to_json()));
    }
    if !result.ready() {
        std::process::exit(1);
    }
}

pub fn worktree_done_cmd(feature: &str, force: bool, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let result = wt_ok(crate::worktree::done(&anchor, feature, force, &cwd));
    if !plain {
        println!("{}", json_pretty(&result.to_json()));
        return;
    }
    if !result.status_summary.is_empty() {
        eprintln!("{}", result.status_summary);
    }
    println!("removed {}", result.removed_path);
    println!(
        "branch {} kept (delete after PR merge via normal flow)",
        result.branch
    );
}

pub fn worktree_status_cmd(feature: Option<&str>, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let payload: Value = match feature.filter(|f| !f.is_empty()) {
        Some(feature) => {
            serde_json::to_value(wt_ok(crate::worktree::feature_status(&anchor, feature)))
                .unwrap_or(Value::Null)
        }
        None => serde_json::to_value(wt_ok(crate::worktree::pool_status(&anchor)))
            .unwrap_or(Value::Null),
    };
    if !plain {
        println!("{}", json_pretty(&payload));
        return;
    }
    let rows: Vec<Value> = match payload {
        Value::Array(rows) => rows,
        other => vec![other],
    };
    if rows.is_empty() {
        println!("no hive-labeled worktrees or branches");
        return;
    }
    for row in rows {
        let row = match row.as_object() {
            Some(row) => row.clone(),
            None => continue,
        };
        let mut flags: Vec<String> = Vec::new();
        if is_set(row.get("dirty")) {
            flags.push("dirty".to_string());
        }
        let in_progress: Vec<String> = row
            .get("inProgress")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !in_progress.is_empty() {
            flags.push(format!("in-progress:{}", in_progress.join(",")));
        }
        if is_set(row.get("stale")) {
            flags.push("stale".to_string());
        }
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(" "))
        };
        let owner_value = map_str(&row, "owner");
        let owner = if owner_value.is_empty() {
            String::new()
        } else {
            format!(" owner={owner_value}")
        };
        let line = format!(
            "{}: {}{owner} {}{suffix}",
            map_str(&row, "feature"),
            map_str(&row, "state"),
            map_str(&row, "worktreePath")
        );
        println!("{}", line.trim_end());
    }
}
