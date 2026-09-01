use std::os::unix::process::CommandExt;

use serde_json::Value;

use super::*;

// ---------------------------------------------------------------------------
// Python-compatible output helpers
// ---------------------------------------------------------------------------

/// `json.dumps(value, ...)` — Python's separators (", ", ": "), optional
/// `indent`, `sort_keys`, and `ensure_ascii` \uXXXX escaping.
pub(crate) fn py_dumps(
    value: &Value,
    ensure_ascii: bool,
    indent: Option<usize>,
    sort_keys: bool,
) -> String {
    let mut out = String::new();
    write_py_value(&mut out, value, ensure_ascii, indent, sort_keys, 0);
    out
}

fn write_py_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7f => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_py_value(
    out: &mut String,
    value: &Value,
    ensure_ascii: bool,
    indent: Option<usize>,
    sort_keys: bool,
    level: usize,
) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_py_string(out, s, ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            match indent {
                None => {
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_py_value(out, item, ensure_ascii, indent, sort_keys, level);
                    }
                    out.push(']');
                }
                Some(width) => {
                    out.push('[');
                    let pad = " ".repeat(width * (level + 1));
                    for (i, item) in items.iter().enumerate() {
                        out.push_str(if i > 0 { ",\n" } else { "\n" });
                        out.push_str(&pad);
                        write_py_value(out, item, ensure_ascii, indent, sort_keys, level + 1);
                    }
                    out.push('\n');
                    out.push_str(&" ".repeat(width * level));
                    out.push(']');
                }
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            match indent {
                None => {
                    out.push('{');
                    for (i, key) in keys.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_py_string(out, key, ensure_ascii);
                        out.push_str(": ");
                        write_py_value(out, &map[*key], ensure_ascii, indent, sort_keys, level);
                    }
                    out.push('}');
                }
                Some(width) => {
                    out.push('{');
                    let pad = " ".repeat(width * (level + 1));
                    for (i, key) in keys.iter().enumerate() {
                        out.push_str(if i > 0 { ",\n" } else { "\n" });
                        out.push_str(&pad);
                        write_py_string(out, key, ensure_ascii);
                        out.push_str(": ");
                        write_py_value(out, &map[*key], ensure_ascii, indent, sort_keys, level + 1);
                    }
                    out.push('\n');
                    out.push_str(&" ".repeat(width * level));
                    out.push('}');
                }
            }
        }
    }
}

/// Python `shlex.quote`.
pub(crate) fn shlex_quote(value: &str) -> String {
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

/// `str(uuid.uuid4())`.
pub(super) fn uuid4() -> String {
    let b = os_random_bytes(16);
    let mut b: Vec<u8> = b;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

/// `os.execvp` — replace this process; print the error and exit 1 on failure.
pub(super) fn execvp(program: &str, args: &[String]) -> ! {
    let err = std::process::Command::new(program).args(args).exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

pub(super) fn py_isprintable(s: &str) -> bool {
    // ponytail: control-char gate covers the documented threats (ESC/OSC/BEL/
    // newline); the full Unicode C*/Z* table of str.isprintable is overkill.
    s.chars()
        .all(|c| !c.is_control() && c != '\u{2028}' && c != '\u{2029}')
}

pub(super) fn stdout_isatty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

pub(super) fn value_as_env_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
