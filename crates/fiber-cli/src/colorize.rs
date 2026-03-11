//! Colorized JSON and YAML output for serde_json::Value.
//!
//! Color scheme (matching ckb-cli):
//!   - Keys:     blue
//!   - Strings:  green
//!   - Numbers:  magenta
//!   - Booleans: yellow
//!   - Null:     cyan

use colored::Colorize;
use serde_json::Value;
use std::fmt::Write;

/// Render a JSON Value as a colorized pretty-printed JSON string.
pub fn colorize_json(value: &Value) -> String {
    let mut buf = String::new();
    write_json_value(&mut buf, value, 0);
    buf
}

/// Render a JSON Value as a colorized YAML string.
pub fn colorize_yaml(value: &Value) -> String {
    let mut buf = String::new();
    write_yaml_value(&mut buf, value, 0, true);
    // Remove trailing newline for consistency with println!
    if buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

// ── JSON colorizer ──────────────────────────────────────────────────────

fn write_json_value(buf: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Null => {
            let _ = write!(buf, "{}", "null".cyan());
        }
        Value::Bool(b) => {
            let _ = write!(buf, "{}", b.to_string().yellow());
        }
        Value::Number(n) => {
            let _ = write!(buf, "{}", n.to_string().magenta());
        }
        Value::String(s) => {
            let _ = write!(buf, "{}", format!("\"{}\"", escape_json_str(s)).green());
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                let _ = write!(buf, "[]");
                return;
            }
            buf.push_str("[\n");
            for (i, item) in arr.iter().enumerate() {
                write_indent(buf, indent + 1);
                write_json_value(buf, item, indent + 1);
                if i + 1 < arr.len() {
                    buf.push(',');
                }
                buf.push('\n');
            }
            write_indent(buf, indent);
            buf.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                let _ = write!(buf, "{{}}");
                return;
            }
            buf.push_str("{\n");
            let len = map.len();
            for (i, (key, val)) in map.iter().enumerate() {
                write_indent(buf, indent + 1);
                let _ = write!(buf, "{}: ", format!("\"{}\"", escape_json_str(key)).blue());
                write_json_value(buf, val, indent + 1);
                if i + 1 < len {
                    buf.push(',');
                }
                buf.push('\n');
            }
            write_indent(buf, indent);
            buf.push('}');
        }
    }
}

fn write_indent(buf: &mut String, level: usize) {
    for _ in 0..level {
        buf.push_str("  ");
    }
}

fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ── YAML colorizer ──────────────────────────────────────────────────────

fn write_yaml_value(buf: &mut String, value: &Value, indent: usize, is_root: bool) {
    match value {
        Value::Null => {
            let _ = writeln!(buf, "{}", "~".cyan());
        }
        Value::Bool(b) => {
            let _ = writeln!(buf, "{}", b.to_string().yellow());
        }
        Value::Number(n) => {
            let _ = writeln!(buf, "{}", n.to_string().magenta());
        }
        Value::String(s) => {
            write_yaml_string(buf, s);
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                let _ = writeln!(buf, "[]");
                return;
            }
            // If this array is a value in a mapping, the first "- " goes on a new line
            if !is_root {
                buf.push('\n');
            }
            for item in arr {
                write_yaml_indent(buf, indent);
                buf.push_str("- ");
                write_yaml_value(buf, item, indent + 1, false);
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                let _ = writeln!(buf, "{{}}");
                return;
            }
            if !is_root {
                buf.push('\n');
            }
            for (key, val) in map {
                write_yaml_indent(buf, indent);
                let _ = write!(buf, "{}: ", yaml_key(key).blue());
                match val {
                    Value::Object(_) | Value::Array(_) => {
                        write_yaml_value(buf, val, indent + 1, false);
                    }
                    _ => {
                        write_yaml_value(buf, val, indent, false);
                    }
                }
            }
        }
    }
}

fn write_yaml_indent(buf: &mut String, level: usize) {
    for _ in 0..level {
        buf.push_str("  ");
    }
}

/// Format a YAML key, quoting if necessary.
fn yaml_key(key: &str) -> String {
    if yaml_needs_quoting(key) {
        format!("\"{}\"", escape_json_str(key))
    } else {
        key.to_string()
    }
}

/// Format a YAML string value, quoting if necessary.
fn write_yaml_string(buf: &mut String, s: &str) {
    if s.is_empty() {
        let _ = writeln!(buf, "{}", "\"\"".green());
    } else if yaml_needs_quoting(s) {
        let _ = writeln!(buf, "{}", format!("\"{}\"", escape_json_str(s)).green());
    } else {
        let _ = writeln!(buf, "{}", s.green());
    }
}

/// Check if a YAML string needs quoting.
fn yaml_needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    // Reserved YAML words
    let lower = s.to_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
    ) {
        return true;
    }
    // Starts/ends with whitespace
    if s.starts_with(' ') || s.ends_with(' ') {
        return true;
    }
    // Contains characters that need quoting
    s.contains(|c: char| {
        matches!(
            c,
            ':' | '#'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '\''
                | '"'
                | '%'
                | '@'
                | '`'
                | '\n'
                | '\r'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── yaml_needs_quoting tests ─────────────────────────────────────────

    #[test]
    fn test_yaml_quoting_empty_string() {
        assert!(yaml_needs_quoting(""));
    }

    #[test]
    fn test_yaml_quoting_reserved_words() {
        assert!(yaml_needs_quoting("true"));
        assert!(yaml_needs_quoting("false"));
        assert!(yaml_needs_quoting("null"));
        assert!(yaml_needs_quoting("yes"));
        assert!(yaml_needs_quoting("no"));
        assert!(yaml_needs_quoting("on"));
        assert!(yaml_needs_quoting("off"));
        assert!(yaml_needs_quoting("~"));
    }

    #[test]
    fn test_yaml_quoting_reserved_words_case_insensitive() {
        assert!(yaml_needs_quoting("True"));
        assert!(yaml_needs_quoting("FALSE"));
        assert!(yaml_needs_quoting("Null"));
        assert!(yaml_needs_quoting("YES"));
    }

    #[test]
    fn test_yaml_quoting_leading_trailing_spaces() {
        assert!(yaml_needs_quoting(" hello"));
        assert!(yaml_needs_quoting("hello "));
    }

    #[test]
    fn test_yaml_quoting_special_chars() {
        assert!(yaml_needs_quoting("key: value"));
        assert!(yaml_needs_quoting("# comment"));
        assert!(yaml_needs_quoting("[array]"));
        assert!(yaml_needs_quoting("{object}"));
        assert!(yaml_needs_quoting("a, b"));
        assert!(yaml_needs_quoting("a & b"));
        assert!(yaml_needs_quoting("*ref"));
        assert!(yaml_needs_quoting("!tag"));
        assert!(yaml_needs_quoting("a | b"));
        assert!(yaml_needs_quoting("a > b"));
        assert!(yaml_needs_quoting("it's"));
        assert!(yaml_needs_quoting(r#"say "hi""#));
        assert!(yaml_needs_quoting("100%"));
        assert!(yaml_needs_quoting("@mention"));
        assert!(yaml_needs_quoting("`code`"));
        assert!(yaml_needs_quoting("line\nbreak"));
        assert!(yaml_needs_quoting("return\rchar"));
    }

    #[test]
    fn test_yaml_quoting_safe_strings() {
        assert!(!yaml_needs_quoting("hello"));
        assert!(!yaml_needs_quoting("simple-string"));
        assert!(!yaml_needs_quoting("123"));
        assert!(!yaml_needs_quoting("0xabcdef"));
        assert!(!yaml_needs_quoting("path/to/file"));
    }

    // ── escape_json_str tests ────────────────────────────────────────────

    #[test]
    fn test_escape_json_str_plain() {
        assert_eq!(escape_json_str("hello"), "hello");
    }

    #[test]
    fn test_escape_json_str_special_chars() {
        assert_eq!(escape_json_str("he said \"hi\""), r#"he said \"hi\""#);
        assert_eq!(escape_json_str("back\\slash"), r"back\\slash");
        assert_eq!(escape_json_str("new\nline"), r"new\nline");
        assert_eq!(escape_json_str("tab\there"), r"tab\there");
        assert_eq!(escape_json_str("return\rchar"), r"return\rchar");
    }

    #[test]
    fn test_escape_json_str_control_chars() {
        assert_eq!(escape_json_str("\x00"), r"\u0000");
        assert_eq!(escape_json_str("\x1f"), r"\u001f");
    }

    // ── colorize_json round-trip structure tests ─────────────────────────
    // Note: We can't easily test exact ANSI color codes, but we can verify
    // that the output contains the expected text content.

    #[test]
    fn test_colorize_json_null() {
        let result = colorize_json(&json!(null));
        assert!(result.contains("null"));
    }

    #[test]
    fn test_colorize_json_bool() {
        let result = colorize_json(&json!(true));
        assert!(result.contains("true"));
    }

    #[test]
    fn test_colorize_json_number() {
        let result = colorize_json(&json!(42));
        assert!(result.contains("42"));
    }

    #[test]
    fn test_colorize_json_string() {
        let result = colorize_json(&json!("hello"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_colorize_json_empty_array() {
        let result = colorize_json(&json!([]));
        assert!(result.contains("[]"));
    }

    #[test]
    fn test_colorize_json_empty_object() {
        let result = colorize_json(&json!({}));
        assert!(result.contains("{}"));
    }

    #[test]
    fn test_colorize_json_object_has_keys() {
        let result = colorize_json(&json!({"name": "alice", "age": 30}));
        assert!(result.contains("name"));
        assert!(result.contains("alice"));
        assert!(result.contains("age"));
        assert!(result.contains("30"));
    }

    // ── colorize_yaml structure tests ────────────────────────────────────

    #[test]
    fn test_colorize_yaml_null() {
        let result = colorize_yaml(&json!(null));
        assert!(result.contains("~"));
    }

    #[test]
    fn test_colorize_yaml_bool() {
        let result = colorize_yaml(&json!(false));
        assert!(result.contains("false"));
    }

    #[test]
    fn test_colorize_yaml_number() {
        let result = colorize_yaml(&json!(99));
        assert!(result.contains("99"));
    }

    #[test]
    fn test_colorize_yaml_string() {
        let result = colorize_yaml(&json!("world"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_colorize_yaml_empty_array() {
        let result = colorize_yaml(&json!([]));
        assert!(result.contains("[]"));
    }

    #[test]
    fn test_colorize_yaml_empty_object() {
        let result = colorize_yaml(&json!({}));
        assert!(result.contains("{}"));
    }

    #[test]
    fn test_colorize_yaml_object_has_keys() {
        let result = colorize_yaml(&json!({"key": "value"}));
        assert!(result.contains("key"));
        assert!(result.contains("value"));
    }
}
