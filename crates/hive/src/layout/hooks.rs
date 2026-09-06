//! The two window hooks that make every layout event reach the planner
//! without a hived: `window-resized` (a client attaching at another size,
//! `resize-window`) and `window-layout-changed` (a split, a kill-pane, an
//! unzoom, a border drag, and hive's own `select-layout`). Both run
//! `hive layout auto --on-change --window <id>`, which compares the plan
//! it would apply against `@hive-layout` and writes nothing when they
//! match — what keeps a drag alive and stops the hook from recursing —
//! and yields to an apply already holding the window lock (`mod.rs`),
//! so a drag firing the hook per step never queues processes.

use crate::tmux::run;

pub const LAYOUT_HOOKS: [&str; 2] = ["window-resized", "window-layout-changed"];

/// The shell line both hooks run. A run-shell job carries no TMUX_PANE, so
/// the window travels as an argument; run-shell expands the format. `hive`
/// is already shell-quoted; the tmux double-quote parser gets its own
/// escaping on top, or a `$` in the path would be expanded away. Output
/// is discarded: run-shell shows any stdout in view mode over the active
/// pane, a member's TUI, until someone presses q, and a nonzero exit
/// the same way; a hook firing per drag step must never do that.
fn layout_run_shell(hive: &str) -> String {
    let hive = crate::shell::tmux_dquote_escape(hive);
    format!(
        "run-shell -b \"{hive} layout auto --on-change --window '#{{window_id}}' >/dev/null 2>&1 || true\""
    )
}

/// `set-hook` argv for the two hooks on `window`, `hive` being the
/// shell-quoted binary path.
pub fn hook_argv(window: &str, hive: &str) -> Vec<Vec<String>> {
    LAYOUT_HOOKS
        .iter()
        .map(|hook| {
            vec![
                "set-hook".to_string(),
                "-w".to_string(),
                "-t".to_string(),
                window.to_string(),
                hook.to_string(),
                layout_run_shell(hive),
            ]
        })
        .collect()
}

/// `set-hook -u` argv unsetting the two hooks on `window`.
pub fn unhook_argv(window: &str) -> Vec<Vec<String>> {
    LAYOUT_HOOKS
        .iter()
        .map(|hook| {
            vec![
                "set-hook".to_string(),
                "-w".to_string(),
                "-u".to_string(),
                "-t".to_string(),
                window.to_string(),
                hook.to_string(),
            ]
        })
        .collect()
}

fn run_rows(rows: Vec<Vec<String>>) {
    for row in rows {
        let args: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = run(&args, false, 5);
    }
}

/// Install the hooks on `window` (idempotent: plain sets).
pub fn install_hooks(window: &str) {
    let hive = crate::shell::shlex_quote(&crate::paths::self_exe());
    run_rows(hook_argv(window, &hive));
}

/// Unset the hooks on `window` and drop its `@hive-layout`: a window that
/// stops being hive's (`hive delete` on a window the human's session lent
/// the team, a stale tag cleared) keeps its layout to itself from then on.
/// Hooks persist on the server across binaries, so a tag sweep that left
/// them would keep re-tiling the human's panes forever.
pub fn remove_hooks(window: &str) {
    run_rows(unhook_argv(window));
    crate::tmux::clear_window_option(window, super::LAYOUT_KEY_OPTION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_argv_sets_both_window_hooks_naming_the_window_id() {
        let rows = hook_argv("dev:1", "/x/hive");
        let run = "run-shell -b \"/x/hive layout auto --on-change --window '#{window_id}' >/dev/null 2>&1 || true\"";
        assert_eq!(
            rows,
            vec![
                vec!["set-hook", "-w", "-t", "dev:1", "window-resized", run],
                vec![
                    "set-hook",
                    "-w",
                    "-t",
                    "dev:1",
                    "window-layout-changed",
                    run
                ],
            ]
        );
    }

    #[test]
    fn test_hook_argv_keeps_a_quoted_binary_path_inside_the_run_shell_string() {
        let rows = hook_argv("@7", "'/Users/a b/hive'");
        assert_eq!(rows[0][3], "@7");
        assert!(
            rows[1][5].starts_with("run-shell -b \"'/Users/a b/hive' layout auto"),
            "{}",
            rows[1][5]
        );
    }

    #[test]
    fn test_hook_argv_escapes_a_dollar_in_the_binary_path_for_tmux() {
        // tmux expands `$x` inside its double quotes whatever the shell
        // quoting around it; escaped, the path reaches sh intact.
        let rows = hook_argv("@7", "'/tmp/we ird$x/hive'");
        assert!(
            rows[0][5].starts_with("run-shell -b \"'/tmp/we ird\\$x/hive' layout auto"),
            "{}",
            rows[0][5]
        );
        assert!(
            rows[0][5].ends_with("--window '#{window_id}' >/dev/null 2>&1 || true\""),
            "{}",
            rows[0][5]
        );
    }

    #[test]
    fn test_unhook_argv_unsets_both_window_hooks() {
        assert_eq!(
            unhook_argv("dev:1"),
            vec![
                vec!["set-hook", "-w", "-u", "-t", "dev:1", "window-resized"],
                vec![
                    "set-hook",
                    "-w",
                    "-u",
                    "-t",
                    "dev:1",
                    "window-layout-changed"
                ],
            ]
        );
    }
}
