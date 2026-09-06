//! Field reads over the JSON documents hive round-trips as
//! `serde_json::Value` (registry entries, claude/codex session records,
//! plugin state). Every layer may depend on this module; it depends on
//! nothing in the crate.

use serde_json::Value;

/// The field is present and carries something: a non-empty string, array
/// or object, `true`, or any number. `null`, `false`, `""`, `[]`, `{}` and
/// a missing field are not set.
pub fn is_set(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(_)) => true,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_set_for_every_json_kind() {
        assert!(!is_set(None));
        assert!(!is_set(Some(&Value::Null)));
        assert!(!is_set(Some(&json!(false))));
        assert!(is_set(Some(&json!(true))));
        assert!(is_set(Some(&json!(0))));
        assert!(!is_set(Some(&json!(""))));
        assert!(is_set(Some(&json!("x"))));
        assert!(!is_set(Some(&json!([]))));
        assert!(is_set(Some(&json!([1]))));
        assert!(!is_set(Some(&json!({}))));
        assert!(is_set(Some(&json!({"a": 1}))));
    }
}
