//! Python value semantics the on-disk documents still bind to
//! (`crates/hive/PORTING.md`): truthiness over a JSON value and `str(float)`.
//! Every layer may depend on this module; it depends on nothing in the crate.

use serde_json::Value;

/// Python truthiness for an optional JSON value.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str(float)`: integral floats keep a trailing `.0`. Registry
/// `createdAt` is compared as a string, so every writer formats it this way.
// ponytail: no scientific-notation branch — epoch timestamps never reach 1e16.
pub(crate) fn py_float_str(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_truthy_follows_python_for_every_json_kind() {
        for (value, expected) in [
            (json!(null), false),
            (json!(false), false),
            (json!(true), true),
            (json!(0), false),
            (json!(0.0), false),
            (json!(-1), true),
            (json!(""), false),
            (json!("0"), true),
            (json!([]), false),
            (json!([0]), true),
            (json!({}), false),
            (json!({"k": null}), true),
        ] {
            assert_eq!(truthy(Some(&value)), expected, "{value}");
        }
        assert!(!truthy(None));
    }

    #[test]
    fn test_py_float_str_keeps_the_trailing_zero_on_whole_seconds() {
        assert_eq!(py_float_str(1700000000.0), "1700000000.0");
        assert_eq!(py_float_str(1700000000.25), "1700000000.25");
        assert_eq!(py_float_str(0.0), "0.0");
        assert_eq!(py_float_str(-2.0), "-2.0");
        assert_eq!(py_float_str(f64::NAN), "NaN");
    }
}
