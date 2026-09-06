use hive::*;

/// Under the C/POSIX locale, give child processes `LC_CTYPE=C.UTF-8`.
///
/// tmux sanitizes control characters (the tab field separators of our `-F`
/// formats) to `_` under a non-UTF-8 locale, so without this the pane/window
/// parses silently lose every `@hive-*` field. Same triage as CPython's PEP
/// 538 coercion: only when LC_ALL is unset and the effective LC_CTYPE is
/// C, POSIX or empty.
fn coerce_c_locale() {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    if !get("LC_ALL").is_empty() {
        return;
    }
    // ponytail: effective LC_CTYPE = LC_CTYPE else LANG; full setlocale
    // resolution is not needed for the C/POSIX/unset triage.
    let ctype = get("LC_CTYPE");
    let effective = if ctype.is_empty() { get("LANG") } else { ctype };
    if matches!(effective.as_str(), "" | "C" | "POSIX") {
        std::env::set_var("LC_CTYPE", "C.UTF-8");
    }
}

fn main() {
    coerce_c_locale();
    // The spawned daemon re-enters this binary as `hive --hived <workspace>
    // <team> <tmux_window> <tmux_window_id>` — route before clap parsing.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--hived") {
        std::process::exit(hived::run_spawned_hived(&args));
    }
    cli::main();
}
