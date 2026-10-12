//! Formatting helpers for command output.

use toml::{Table, Value};

/// What masked values are replaced with.
pub const MASK: &str = "********";

/// Whether a config key looks like it holds a credential.
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["key", "secret", "token"].iter().any(|s| k.contains(s))
}

/// Returns a copy of `table` where string values under sensitive keys are masked.
///
/// Only strings are masked: numbers and booleans under keys such as
/// `max_output_tokens` are settings, not secrets. Everything below a sensitive key
/// (nested tables, arrays) is masked too.
pub fn mask_table(table: &Table) -> Table {
    table
        .iter()
        .map(|(k, v)| (k.clone(), mask_value(v, is_sensitive_key(k))))
        .collect()
}

fn mask_value(value: &Value, sensitive: bool) -> Value {
    match value {
        Value::String(_) if sensitive => Value::String(MASK.into()),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| mask_value(v, sensitive)).collect())
        }
        Value::Table(t) => Value::Table(
            t.iter()
                .map(|(k, v)| (k.clone(), mask_value(v, sensitive || is_sensitive_key(k))))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `1234567` → `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The first non-empty line of a description.
pub fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn masks_only_sensitive_strings() {
        let t: Table = r#"
            api_key = "sk-123"
            name = "visible"
            [model]
            max_output_tokens = 100
            [plugins.telemetry]
            secret_key = "s"
            public = "p"
            [plugins.tokens]
            list = ["a", "b"]
        "#
        .parse()
        .unwrap();
        let m = mask_table(&t);
        assert_eq!(m["api_key"].as_str(), Some(MASK));
        assert_eq!(m["name"].as_str(), Some("visible"));
        assert_eq!(m["model"]["max_output_tokens"].as_integer(), Some(100));
        assert_eq!(m["plugins"]["telemetry"]["secret_key"].as_str(), Some(MASK));
        assert_eq!(m["plugins"]["telemetry"]["public"].as_str(), Some("p"));
        assert_eq!(
            m["plugins"]["tokens"]["list"].as_array().unwrap()[0].as_str(),
            Some(MASK)
        );
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line("\n\n  Reads a file.\nMore"), "Reads a file.");
        assert_eq!(first_line(""), "");
    }
}
