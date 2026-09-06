//! Send-message helpers shared by the hived and the CLI: body-length
//! hints, the `<HIVE>` envelope, and the thread-event projection.

use serde_json::{Map, Value};

const _BODY_WARNING_CHAR_LIMIT: usize = 500;
const _BODY_WARNING_LINE_LIMIT: usize = 3;
const _BODY_WARNING_MARKERS: [&str; 3] = ["# ", "- ", "* "];

#[derive(Debug, Clone, PartialEq)]
pub struct BodyWarningHint {
    pub chars: usize,
    pub lines: usize,
    pub reasons: Vec<&'static str>,
}

/// Split lines over \n, \r\n, and \r.
// ponytail: skips exotic unicode line breaks (\v, \f, \x85, U+2028...);
// extend if a real body ever carries them.
fn splitlines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Suggest when a message body looks better suited for an artifact.
pub fn body_warning_hint(body: &str) -> Option<BodyWarningHint> {
    let text = body.trim();
    if text.is_empty() {
        return None;
    }
    let lines = splitlines(text);
    let mut reasons: Vec<&'static str> = Vec::new();
    if text.chars().count() > _BODY_WARNING_CHAR_LIMIT {
        reasons.push("chars");
    }
    if lines.len() >= _BODY_WARNING_LINE_LIMIT {
        reasons.push("lines");
    }
    if text.contains("```") {
        reasons.push("fenced_code");
    }
    if lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .any(|line| {
            let stripped = line.trim_start();
            _BODY_WARNING_MARKERS
                .iter()
                .any(|marker| stripped.starts_with(marker))
        })
    {
        reasons.push("markdown");
    }
    if reasons.is_empty() {
        return None;
    }
    Some(BodyWarningHint {
        chars: text.chars().count(),
        lines: lines.len(),
        reasons,
    })
}

/// Render the stderr hint for long or structured message bodies.
pub fn format_body_warning(command: &str, hint: &BodyWarningHint) -> String {
    let mut summary = vec![
        format!("{} chars", hint.chars),
        format!("{} lines", hint.lines),
    ];
    if hint.reasons.contains(&"fenced_code") {
        summary.push("fenced code".to_string());
    }
    if hint.reasons.contains(&"markdown") {
        summary.push("markdown".to_string());
    }
    let details = summary.join(", ");
    format!(
        "warning: body looks long or structured ({details}); consider stdin artifact:\n  hive {command} <agent> \"<short summary>\" --artifact - <<'EOF'\n  ...\n  EOF"
    )
}

/// Project durable send events into the smaller thread-facing shape.
pub fn project_thread_event(event: &Map<String, Value>) -> Map<String, Value> {
    let mut projected = Map::new();
    for key in [
        "from",
        "to",
        "intent",
        "metadata",
        "createdAt",
        "msgId",
        "inReplyTo",
        "body",
        "artifact",
    ] {
        match event.get(key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) if s.is_empty() => continue,
            Some(value) => {
                projected.insert(key.to_string(), value.clone());
            }
        }
    }
    projected
}

pub fn format_hive_envelope(
    from_agent: &str,
    to_agent: &str,
    body: &str,
    artifact: &str,
    message_id: &str,
    reply_to: &str,
) -> String {
    let mut attrs: Vec<(&str, &str)> = vec![("from", from_agent), ("to", to_agent)];
    if !message_id.is_empty() {
        attrs.push(("msgId", message_id));
    }
    if !reply_to.is_empty() {
        attrs.push(("reply-to", reply_to));
    }
    if !artifact.is_empty() {
        attrs.push(("artifact", artifact));
    }
    let header = format!(
        "<HIVE {}>",
        attrs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let trimmed = body.trim();
    let payload = if trimmed.is_empty() {
        "(no message)"
    } else {
        trimmed
    };
    format!("{header}\n{payload}\n</HIVE>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_body_warning_hint_flags_structured_body() {
        assert_eq!(body_warning_hint("short line"), None);
        assert_eq!(body_warning_hint("   "), None);
        let hint = body_warning_hint("# title\nline two\nline three").unwrap();
        assert_eq!(hint.chars, 27);
        assert_eq!(hint.lines, 3);
        assert_eq!(hint.reasons, vec!["lines", "markdown"]);
        let warning = format_body_warning("send", &hint);
        assert!(warning.starts_with(
            "warning: body looks long or structured (27 chars, 3 lines, markdown); consider stdin artifact:\n  hive send <agent>"
        ));
        assert!(warning.ends_with("--artifact - <<'EOF'\n  ...\n  EOF"));
    }

    #[test]
    fn test_format_hive_envelope_orders_attrs_and_defaults_body() {
        assert_eq!(
            format_hive_envelope("a.b", "c.d", "  hi  ", "art.md", "m1", "m0"),
            "<HIVE from=a.b to=c.d msgId=m1 reply-to=m0 artifact=art.md>\nhi\n</HIVE>"
        );
        assert_eq!(
            format_hive_envelope("a.b", "c.d", "", "", "", ""),
            "<HIVE from=a.b to=c.d>\n(no message)\n</HIVE>"
        );
    }

    #[test]
    fn test_project_thread_event_drops_empty_values() {
        let event = json!({
            "from": "a.b",
            "to": "",
            "intent": null,
            "msgId": "m1",
            "extra": "dropped",
            "metadata": {"k": "v"}
        });
        let projected = project_thread_event(event.as_object().unwrap());
        assert_eq!(
            Value::Object(projected),
            json!({"from": "a.b", "msgId": "m1", "metadata": {"k": "v"}})
        );
    }
}
