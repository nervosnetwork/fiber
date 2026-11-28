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
fn test_yaml_quoting_with_control_chars() {
    for c in '\0'..='\x1f' {
        let s = c.to_string();
        assert!(
            yaml_needs_quoting(&s),
            "Control character U+{:04X} should require quoting",
            c as u32
        );
    }

    for n in 0x7f..=0x9f {
        let c = char::from_u32(n).unwrap_or('\u{FFFD}');
        let s = c.to_string();
        assert!(
            yaml_needs_quoting(&s),
            "Control character U+{:04X} should require quoting",
            n
        );
    }
    for c in 'a'..='z' {
        let s = c.to_string();
        assert!(
            !yaml_needs_quoting(&s),
            "Printable character '{}' should not require quoting",
            c
        );
    }
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
