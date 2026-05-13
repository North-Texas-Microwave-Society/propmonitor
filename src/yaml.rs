//! Minimal YAML parser tailored to propmonitor's `config.yaml` schema.
//!
//! Supports: line comments (`#`), `key: value` scalars, two-level nesting
//! via indentation, optional double-quoted strings. No flow style, no
//! anchors, no multi-line scalars, no lists. Anything fancier produces a
//! parse error rather than silently misbehaving.
//!
//! The shape we accept:
//!
//! ```yaml
//! frequency: 28330000
//! mode: beacon
//! beacon:
//!   offset_hz: 0
//!   bandwidth_hz: 50
//! ```

use std::collections::BTreeMap;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub enum Value {
    Scalar(String),
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Value::Scalar(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }
}

/// Parse a YAML document into the top-level mapping.
pub fn parse(text: &str) -> Result<BTreeMap<String, Value>> {
    let mut top: BTreeMap<String, Value> = BTreeMap::new();
    // The currently-open child mapping, if the previous top-level key
    // declared an empty value (i.e. expects an indented block).
    let mut open_child: Option<(String, BTreeMap<String, Value>)> = None;

    for (lineno, raw_line) in text.lines().enumerate() {
        let line_no = lineno + 1;
        let stripped = strip_comment(raw_line);
        if stripped.trim().is_empty() {
            continue;
        }

        let indent = stripped.len() - stripped.trim_start().len();
        let content = stripped.trim_start();

        match indent {
            0 => {
                // Closing the previous child block, if any.
                if let Some((k, m)) = open_child.take() {
                    top.insert(k, Value::Map(m));
                }
                let (key, value) = split_kv(content, line_no)?;
                if value.is_empty() {
                    open_child = Some((key, BTreeMap::new()));
                } else {
                    top.insert(key, Value::Scalar(unquote(value)));
                }
            }
            2 => {
                let child = open_child.as_mut().ok_or_else(|| {
                    Error::msg(format!(
                        "yaml line {}: indented entry without a parent key",
                        line_no
                    ))
                })?;
                let (key, value) = split_kv(content, line_no)?;
                if value.is_empty() {
                    return Err(Error::msg(format!(
                        "yaml line {}: nested mappings deeper than one level are not supported",
                        line_no
                    )));
                }
                child.1.insert(key, Value::Scalar(unquote(value)));
            }
            n => {
                return Err(Error::msg(format!(
                    "yaml line {}: unexpected indent {} (only 0 or 2 supported)",
                    line_no, n
                )));
            }
        }
    }
    if let Some((k, m)) = open_child.take() {
        top.insert(k, Value::Map(m));
    }
    Ok(top)
}

/// Strip a `#` comment from a line, respecting double-quoted strings so
/// that `key: "string with # inside"` parses correctly.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == '#' && !in_quotes {
            return &line[..i];
        }
    }
    line
}

fn split_kv(s: &str, line_no: usize) -> Result<(String, &str)> {
    let colon = s.find(':').ok_or_else(|| {
        Error::msg(format!(
            "yaml line {}: expected `key: value` (no colon)",
            line_no
        ))
    })?;
    let key = s[..colon].trim().to_string();
    if key.is_empty() {
        return Err(Error::msg(format!("yaml line {}: empty key", line_no)));
    }
    let value = s[colon + 1..].trim();
    Ok((key, value))
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

// ---------------- typed accessors -------------------------------------

pub fn require<'a>(
    map: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a Value> {
    map.get(key)
        .ok_or_else(|| Error::msg(format!("config: missing required key `{}`", key)))
}

pub fn require_scalar<'a>(
    map: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str> {
    require(map, key)?
        .as_scalar()
        .ok_or_else(|| Error::msg(format!("config: `{}` must be a scalar", key)))
}

pub fn parse_f64(s: &str, key: &str) -> Result<f64> {
    s.parse::<f64>()
        .map_err(|_| Error::msg(format!("config: `{}` is not a number ({:?})", key, s)))
}

pub fn parse_usize(s: &str, key: &str) -> Result<usize> {
    s.parse::<usize>()
        .map_err(|_| Error::msg(format!("config: `{}` is not an integer ({:?})", key, s)))
}

// ---------------- writer ----------------------------------------------

/// Helper for `Config::to_yaml_string` — builds a YAML document one line at
/// a time. Quoting follows the same conventions as the parser: bare scalars
/// for numbers, double-quoted for strings that contain spaces or special
/// characters.
pub struct YamlWriter {
    out: String,
}

impl YamlWriter {
    pub fn new() -> Self {
        Self { out: String::new() }
    }

    pub fn scalar(&mut self, key: &str, value: &str) {
        self.out.push_str(key);
        self.out.push_str(": ");
        self.out.push_str(value);
        self.out.push('\n');
    }

    pub fn string(&mut self, key: &str, value: &str) {
        self.out.push_str(key);
        self.out.push_str(": ");
        self.write_quoted(value);
        self.out.push('\n');
    }

    pub fn nested_open(&mut self, key: &str) {
        self.out.push_str(key);
        self.out.push_str(":\n");
    }

    pub fn nested_scalar(&mut self, key: &str, value: &str) {
        self.out.push_str("  ");
        self.out.push_str(key);
        self.out.push_str(": ");
        self.out.push_str(value);
        self.out.push('\n');
    }

    pub fn nested_string(&mut self, key: &str, value: &str) {
        self.out.push_str("  ");
        self.out.push_str(key);
        self.out.push_str(": ");
        self.write_quoted(value);
        self.out.push('\n');
    }

    fn write_quoted(&mut self, value: &str) {
        self.out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                _ => self.out.push(ch),
            }
        }
        self.out.push('"');
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// Atomically replace `path` with `contents`. Writes to `<path>.tmp` first,
/// fsyncs, then renames over the destination.
pub fn write_atomic(path: &str, contents: &str) -> Result<()> {
    use std::io::Write;
    let tmp = format!("{}.tmp", path);
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| Error::msg(format!("create {}: {}", tmp, e)))?;
    f.write_all(contents.as_bytes())
        .map_err(|e| Error::msg(format!("write {}: {}", tmp, e)))?;
    f.sync_all()
        .map_err(|e| Error::msg(format!("fsync {}: {}", tmp, e)))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::msg(format!("rename {} -> {}: {}", tmp, path, e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_pairs() {
        let m = parse("frequency: 101100000\nmode: wfm\n").unwrap();
        assert_eq!(m.get("frequency").unwrap().as_scalar(), Some("101100000"));
        assert_eq!(m.get("mode").unwrap().as_scalar(), Some("wfm"));
    }

    #[test]
    fn parses_quoted_string_with_spaces() {
        let m = parse("submode: \"60C with spaces\"\n").unwrap();
        assert_eq!(
            m.get("submode").unwrap().as_scalar(),
            Some("60C with spaces")
        );
    }

    #[test]
    fn handles_trailing_and_full_line_comments() {
        let m = parse("# top comment\nfrequency: 50211000 # tuned to 6m beacon\n").unwrap();
        assert_eq!(m.get("frequency").unwrap().as_scalar(), Some("50211000"));
    }

    #[test]
    fn parses_nested_block() {
        let yaml = "\
mode: beacon
beacon:
  offset_hz: 0
  bandwidth_hz: 50
";
        let m = parse(yaml).unwrap();
        let b = m.get("beacon").unwrap().as_map().unwrap();
        assert_eq!(b.get("offset_hz").unwrap().as_scalar(), Some("0"));
        assert_eq!(b.get("bandwidth_hz").unwrap().as_scalar(), Some("50"));
    }

    #[test]
    fn rejects_deeper_indent() {
        let yaml = "beacon:\n    nested: x\n";
        assert!(parse(yaml).is_err());
    }

    #[test]
    fn comment_inside_quotes_is_kept() {
        let m = parse("note: \"a # not a comment\"\n").unwrap();
        assert_eq!(
            m.get("note").unwrap().as_scalar(),
            Some("a # not a comment")
        );
    }
}
