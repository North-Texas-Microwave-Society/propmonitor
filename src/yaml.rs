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
                    insert_unique(&mut top, k, Value::Map(m), line_no)?;
                }
                let (key, value) = split_kv(content, line_no)?;
                if value.is_empty() {
                    open_child = Some((key, BTreeMap::new()));
                } else {
                    insert_unique(
                        &mut top,
                        key,
                        Value::Scalar(unquote(value, line_no)?),
                        line_no,
                    )?;
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
                insert_unique(
                    &mut child.1,
                    key,
                    Value::Scalar(unquote(value, line_no)?),
                    line_no,
                )?;
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
        // `text.lines().count() + 1` points just past the document and
        // gives duplicate-parent errors a useful location.
        insert_unique(&mut top, k, Value::Map(m), text.lines().count() + 1)?;
    }
    Ok(top)
}

/// Strip a `#` comment from a line, respecting double-quoted strings so
/// that `key: "string with # inside"` parses correctly.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && in_quotes {
            escaped = true;
        } else if c == '"' {
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

fn unquote(s: &str, line_no: usize) -> Result<String> {
    if !s.starts_with('"') {
        return Ok(s.to_string());
    }

    if s.len() < 2 || !s.ends_with('"') {
        return Err(Error::msg(format!(
            "yaml line {}: unterminated quoted string",
            line_no
        )));
    }

    let mut out = String::new();
    let mut chars = s[1..s.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                return Err(Error::msg(format!(
                    "yaml line {}: unsupported escape \\{}",
                    line_no, other
                )));
            }
            None => {
                return Err(Error::msg(format!(
                    "yaml line {}: trailing backslash in quoted string",
                    line_no
                )));
            }
        }
    }
    Ok(out)
}

fn insert_unique(
    map: &mut BTreeMap<String, Value>,
    key: String,
    value: Value,
    line_no: usize,
) -> Result<()> {
    if map.contains_key(&key) {
        return Err(Error::msg(format!(
            "yaml line {}: duplicate key {:?}",
            line_no, key
        )));
    }
    map.insert(key, value);
    Ok(())
}

// ---------------- typed accessors -------------------------------------

pub fn require<'a>(map: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a Value> {
    map.get(key)
        .ok_or_else(|| Error::msg(format!("config: missing required key `{}`", key)))
}

pub fn require_scalar<'a>(map: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a str> {
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
    let mut f =
        std::fs::File::create(&tmp).map_err(|e| Error::msg(format!("create {}: {}", tmp, e)))?;
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

    #[test]
    fn quoted_strings_round_trip_escaped_quotes_and_backslashes() {
        let mut writer = YamlWriter::new();
        writer.string("value", "driver=foo\\bar, label=\"test #1\"");
        let parsed = parse(&writer.finish()).unwrap();
        assert_eq!(
            parsed.get("value").unwrap().as_scalar(),
            Some("driver=foo\\bar, label=\"test #1\"")
        );
    }

    #[test]
    fn rejects_malformed_quoted_strings() {
        assert!(parse("value: \"unterminated\n").is_err());
        assert!(parse("value: \"bad\\n escape\"\n").is_err());
    }

    #[test]
    fn rejects_duplicate_keys() {
        assert!(parse("mode: cw\nmode: beacon\n").is_err());
        assert!(parse("beacon:\n  offset_hz: 0\n  offset_hz: 1\n").is_err());
        assert!(parse("beacon:\n  offset_hz: 0\nbeacon:\n  offset_hz: 1\n").is_err());
    }

    #[test]
    fn parse_rejects_line_without_colon() {
        assert!(parse("bare-key\n").is_err());
    }

    #[test]
    fn parse_rejects_empty_key() {
        assert!(parse(": value\n").is_err());
    }

    #[test]
    fn parse_rejects_orphan_nested_entry() {
        // Two-space indent with no parent key on the line above.
        assert!(parse("  orphan: value\n").is_err());
    }

    #[test]
    fn parse_rejects_double_nesting() {
        let yaml = "outer:\n  inner:\n";
        assert!(parse(yaml).is_err());
    }

    #[test]
    fn value_as_scalar_returns_none_for_map() {
        let m = parse("a:\n  b: c\n").unwrap();
        assert!(m.get("a").unwrap().as_scalar().is_none());
    }

    #[test]
    fn value_as_map_returns_none_for_scalar() {
        let m = parse("a: 1\n").unwrap();
        assert!(m.get("a").unwrap().as_map().is_none());
    }

    #[test]
    fn require_returns_missing_key_error() {
        let m = parse("a: 1\n").unwrap();
        assert!(require(&m, "missing").is_err());
        assert!(require(&m, "a").is_ok());
    }

    #[test]
    fn require_scalar_rejects_map_values() {
        let m = parse("a:\n  b: c\n").unwrap();
        assert!(require_scalar(&m, "a").is_err());
    }

    #[test]
    fn parse_f64_rejects_nonsense() {
        assert!(parse_f64("not-a-number", "x").is_err());
        assert_eq!(parse_f64("1.25", "x").unwrap(), 1.25);
    }

    #[test]
    fn parse_usize_rejects_negative() {
        assert!(parse_usize("-5", "x").is_err());
        assert_eq!(parse_usize("42", "x").unwrap(), 42);
    }

    #[test]
    fn writer_scalar_omits_quotes_for_numbers() {
        let mut w = YamlWriter::new();
        w.scalar("frequency", "28330000");
        assert_eq!(w.finish(), "frequency: 28330000\n");
    }

    #[test]
    fn writer_string_quotes_value() {
        let mut w = YamlWriter::new();
        w.string("driver", "rtlsdr,serial=03340219");
        assert_eq!(w.finish(), "driver: \"rtlsdr,serial=03340219\"\n");
    }

    #[test]
    fn writer_nested_block_indents_two_spaces() {
        let mut w = YamlWriter::new();
        w.nested_open("beacon");
        w.nested_scalar("offset_hz", "0");
        w.nested_string("notes", "ten meter");
        assert_eq!(
            w.finish(),
            "beacon:\n  offset_hz: 0\n  notes: \"ten meter\"\n"
        );
    }

    #[test]
    fn writer_escapes_quotes_and_backslashes() {
        let mut w = YamlWriter::new();
        w.string("k", r#"he said "hi" \ ok"#);
        assert_eq!(w.finish(), "k: \"he said \\\"hi\\\" \\\\ ok\"\n");
    }

    #[test]
    fn writer_round_trips_through_parser() {
        let mut w = YamlWriter::new();
        w.scalar("frequency", "28330000");
        w.scalar("mode", "beacon");
        w.string("driver", "rtlsdr");
        w.nested_open("beacon");
        w.nested_scalar("offset_hz", "0");
        w.nested_scalar("bandwidth_hz", "300");
        let yaml = w.finish();
        let parsed = parse(&yaml).unwrap();
        assert_eq!(
            parsed.get("frequency").unwrap().as_scalar(),
            Some("28330000")
        );
        let b = parsed.get("beacon").unwrap().as_map().unwrap();
        assert_eq!(b.get("bandwidth_hz").unwrap().as_scalar(), Some("300"));
    }

    #[test]
    fn write_atomic_replaces_file_contents() {
        // Pick a path in the OS temp dir keyed by the test name + PID so
        // parallel test runs don't collide.
        let path =
            std::env::temp_dir().join(format!("propmonitor-yaml-test-{}.yaml", std::process::id()));
        let path_str = path.to_str().unwrap();
        // Pre-populate so we exercise the rename-over-existing branch.
        std::fs::write(path_str, "old: content\n").unwrap();

        write_atomic(path_str, "new: content\n").unwrap();
        let read_back = std::fs::read_to_string(path_str).unwrap();
        assert_eq!(read_back, "new: content\n");
        let _ = std::fs::remove_file(path_str);
    }

    #[test]
    fn write_atomic_reports_error_on_invalid_path() {
        let err = write_atomic("/no/such/dir/file.yaml", "x").unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("create") || msg.contains("/no/such/dir"));
    }
}
