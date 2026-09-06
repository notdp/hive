//! Quoting for the shell lines hive hands to tmux and to `sh`. Depends
//! on nothing in the crate.

/// POSIX shell quoting: alphanumerics and `_@%+=:,./-` pass through bare,
/// anything else is wrapped in single quotes.
pub fn shlex_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let safe = value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '_' | '-')
    });
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Escape `value` for the inside of a tmux double-quoted string: tmux's
/// command parser expands `$name` and honours `\` there even when the
/// value is single-quoted for the shell it reaches, so a binary path with
/// a `$` would otherwise reach `run-shell` rewritten.
pub fn tmux_dquote_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_tmux_dquote_escape_protects_dollar_backslash_and_quote() {
        assert_eq!(tmux_dquote_escape("/x/hive"), "/x/hive");
        assert_eq!(
            tmux_dquote_escape("'/x/we ird$x/hive'"),
            "'/x/we ird\\$x/hive'"
        );
        assert_eq!(tmux_dquote_escape("a\\b\"c"), "a\\\\b\\\"c");
    }
}
