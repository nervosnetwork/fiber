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

// ── Help text colorizer ──────────────────────────────────────────────────

/// Colorize clap-generated help text.
///
/// Color scheme:
///   - Section headers (Usage:, Commands:, Options:): bold bright_yellow
///   - Command names in Commands section: bright_cyan
///   - Flag names (--flag, -f): bright_green
///   - Placeholders (<value>): cyan
///   - Default values [default: ...]: dimmed
///   - About/description (first line): bright_white
pub fn colorize_help(text: &str) -> String {
    let mut buf = String::with_capacity(text.len() * 2);
    let mut in_commands_section = false;
    let mut in_options_section = false;
    let mut is_first_line = true;

    for line in text.lines() {
        // Detect section headers
        let trimmed = line.trim();
        if trimmed == "Commands:" || trimmed == "Options:" || trimmed == "Arguments:" {
            in_commands_section = trimmed == "Commands:";
            in_options_section = trimmed == "Options:" || trimmed == "Arguments:";
            is_first_line = false;
            let _ = writeln!(buf, "{}", line.bold().bright_yellow());
            continue;
        }

        if trimmed.starts_with("Usage:") {
            is_first_line = false;
            let _ = writeln!(buf, "{}", colorize_usage_line(line));
            continue;
        }

        // Empty line resets section context for separation
        if trimmed.is_empty() {
            is_first_line = false;
            buf.push('\n');
            continue;
        }

        // First non-empty line is the about/description
        if is_first_line {
            is_first_line = false;
            let _ = writeln!(buf, "{}", line.bright_white().bold());
            continue;
        }

        if in_commands_section {
            let _ = writeln!(buf, "{}", colorize_command_line(line));
        } else if in_options_section {
            let _ = writeln!(buf, "{}", colorize_option_line(line));
        } else {
            let _ = writeln!(buf, "{}", line);
        }
    }

    // Remove trailing newline for consistency
    if buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

/// Colorize a "Usage: fnn-cli command [OPTIONS]" line.
fn colorize_usage_line(line: &str) -> String {
    // "Usage: fnn-cli channel [OPTIONS] --pubkey <pubkey>"
    // Color "Usage:" as section header, the rest with flag/placeholder coloring
    if let Some(rest) = line.strip_prefix("Usage: ") {
        let colored_rest = colorize_inline_flags(rest);
        format!("{} {}", "Usage:".bold().bright_yellow(), colored_rest)
    } else if let Some((prefix, rest)) = line.split_once("Usage: ") {
        let colored_rest = colorize_inline_flags(rest);
        format!(
            "{}{} {}",
            prefix,
            "Usage:".bold().bright_yellow(),
            colored_rest
        )
    } else {
        line.bold().bright_yellow().to_string()
    }
}

/// Colorize inline flags and placeholders in a string (for usage lines).
fn colorize_inline_flags(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    let mut current_word = String::new();

    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                // Flush current word
                if !current_word.is_empty() {
                    result.push_str(&colorize_word(&current_word));
                    current_word.clear();
                }
                // Collect placeholder
                let mut placeholder = String::from("<");
                for c in chars.by_ref() {
                    placeholder.push(c);
                    if c == '>' {
                        break;
                    }
                }
                result.push_str(&placeholder.cyan().to_string());
            }
            '[' => {
                // Flush current word
                if !current_word.is_empty() {
                    result.push_str(&colorize_word(&current_word));
                    current_word.clear();
                }
                // Collect bracket content
                let mut bracket = String::from("[");
                let mut depth = 1;
                for c in chars.by_ref() {
                    bracket.push(c);
                    if c == '[' {
                        depth += 1;
                    } else if c == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                result.push_str(&bracket.dimmed().to_string());
            }
            ' ' => {
                if !current_word.is_empty() {
                    result.push_str(&colorize_word(&current_word));
                    current_word.clear();
                }
                result.push(' ');
            }
            _ => {
                current_word.push(ch);
            }
        }
    }
    if !current_word.is_empty() {
        result.push_str(&colorize_word(&current_word));
    }
    result
}

/// Colorize a single word: --flags get green, rest stays white.
fn colorize_word(word: &str) -> String {
    if word.starts_with("--") || word.starts_with('-') && word.len() == 2 {
        word.bright_green().to_string()
    } else {
        word.to_string()
    }
}

/// Colorize a line in the Commands: section.
/// Format: "  command_name   Description text"
fn colorize_command_line(line: &str) -> String {
    // Commands lines have leading whitespace, then the command name, then spaces, then description
    let stripped = line.trim_start();
    let leading_spaces = &line[..line.len() - stripped.len()];

    // Find the boundary between command name and description (2+ spaces)
    if let Some(pos) = stripped.find("  ") {
        let cmd_name = &stripped[..pos];
        let rest = &stripped[pos..];
        format!(
            "{}{}{}",
            leading_spaces,
            cmd_name.bright_cyan(),
            rest.dimmed()
        )
    } else {
        // No description, just command name
        format!("{}{}", leading_spaces, stripped.bright_cyan())
    }
}

/// Colorize a line in the Options: section.
/// Lines can be:
///   - Flag line: "  -f, --flag <value>    Description"
///   - Continuation description: "          More description text"
///   - Default value line: "          [default: value]"
fn colorize_option_line(line: &str) -> String {
    let stripped = line.trim_start();
    let leading_spaces = &line[..line.len() - stripped.len()];

    // Check if this is a flag line (starts with -)
    if stripped.starts_with('-') {
        colorize_flag_line(leading_spaces, stripped)
    } else if stripped.starts_with('[') {
        // Default/possible values annotation
        format!("{}{}", leading_spaces, stripped.dimmed())
    } else {
        // Continuation description
        format!("{}{}", leading_spaces, stripped)
    }
}

/// Colorize a flag definition line like "-f, --flag <value>  Description"
fn colorize_flag_line(leading: &str, stripped: &str) -> String {
    let mut result = String::from(leading);
    let mut chars = stripped.chars().peekable();
    let mut in_description = false;
    let mut consecutive_spaces = 0;

    while let Some(ch) = chars.next() {
        if in_description {
            // Once in description, check for [default: ...] patterns
            result.push(ch);
            // Collect the rest
            let rest: String = chars.collect();
            // Colorize [default: ...] and [possible values: ...] in the description
            let colored = colorize_description_brackets(&format!("{}{}", ch, rest));
            // Remove the first char we already pushed
            result.pop();
            result.push_str(&colored);
            break;
        }

        match ch {
            '-' => {
                // Collect the flag token
                let mut flag = String::from("-");
                let mut flushed = false;
                for c in chars.by_ref() {
                    if c == ' ' || c == ',' {
                        result.push_str(&flag.bright_green().to_string());
                        result.push(c);
                        if c == ' ' {
                            consecutive_spaces = 1;
                        } else {
                            consecutive_spaces = 0;
                        }
                        flushed = true;
                        break;
                    } else if c == '<' {
                        result.push_str(&flag.bright_green().to_string());
                        // Now handle placeholder
                        let mut placeholder = String::from("<");
                        for pc in chars.by_ref() {
                            placeholder.push(pc);
                            if pc == '>' {
                                break;
                            }
                        }
                        result.push_str(&placeholder.cyan().to_string());
                        flushed = true;
                        break;
                    } else {
                        flag.push(c);
                    }
                }
                // Flush if the iterator ended without hitting a delimiter
                if !flushed {
                    result.push_str(&flag.bright_green().to_string());
                }
            }
            '<' => {
                let mut placeholder = String::from("<");
                for c in chars.by_ref() {
                    placeholder.push(c);
                    if c == '>' {
                        break;
                    }
                }
                result.push_str(&placeholder.cyan().to_string());
                consecutive_spaces = 0;
            }
            '[' => {
                let mut bracket = String::from("[");
                let mut depth = 1;
                for c in chars.by_ref() {
                    bracket.push(c);
                    if c == '[' {
                        depth += 1;
                    } else if c == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                result.push_str(&bracket.dimmed().to_string());
                consecutive_spaces = 0;
            }
            ' ' => {
                consecutive_spaces += 1;
                result.push(' ');
                // Two or more consecutive spaces after flags = entering description
                if consecutive_spaces >= 2 {
                    in_description = true;
                }
            }
            _ => {
                consecutive_spaces = 0;
                result.push(ch);
            }
        }
    }

    result
}

/// Colorize [default: ...] and [possible values: ...] brackets in description text.
fn colorize_description_brackets(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        result.push_str(&rest[..start]);
        let after_bracket = &rest[start..];
        if let Some(end) = after_bracket.find(']') {
            let bracket_content = &after_bracket[..=end];
            result.push_str(&bracket_content.dimmed().to_string());
            rest = &after_bracket[end + 1..];
        } else {
            result.push_str(after_bracket);
            return result;
        }
    }
    result.push_str(rest);
    result
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

    // ── colorize_help tests ──────────────────────────────────────────────

    #[test]
    fn test_colorize_help_preserves_content() {
        let help_text = "My CLI tool\n\nUsage: fnn-cli [OPTIONS]\n\nCommands:\n  info  Show node info\n\nOptions:\n  --url <url>  The RPC endpoint\n";
        let result = colorize_help(help_text);
        // All semantic content should be present
        assert!(result.contains("My CLI tool"));
        assert!(result.contains("Usage:"));
        assert!(result.contains("fnn-cli"));
        assert!(result.contains("Commands:"));
        assert!(result.contains("info"));
        assert!(result.contains("Options:"));
        assert!(result.contains("--url"));
    }

    #[test]
    fn test_colorize_help_empty() {
        let result = colorize_help("");
        assert!(result.is_empty());
    }
}
