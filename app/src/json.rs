//! A minimal JSON value: just enough to speak LSP's JSON-RPC framing.
//!
//! Not a general-purpose JSON library (no `serde_json` dependency) -- the
//! wire shapes here are a handful of small, fixed-structure messages
//! (`initialize`, `textDocument/didOpen`, `textDocument/publishDiagnostics`),
//! and hand-rolling a parser and writer for exactly those is a couple hundred
//! lines against a dependency that would otherwise be the only thing in the
//! workspace pulling in `serde_json`.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Key order preserved (a `Vec`, not a map): JSON-RPC objects are small
    /// and read positionally more often than they are looked up, and this
    /// avoids requiring `Hash` on nothing in particular.
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn object(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_f64().map(|n| n.max(0.0) as usize)
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Serializes to compact JSON.
    pub fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => {
                let _ = write!(out, "{n}");
            }
            Value::String(s) => write_escaped(out, s),
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Value::Object(pairs) => {
                out.push('{');
                for (index, (key, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_escaped(out, key);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parses one JSON value, ignoring anything after it (a JSON-RPC message
/// framed by `Content-Length` is exactly one value with nothing trailing).
pub fn parse(text: &str) -> Option<Value> {
    let mut chars = text.chars().peekable();
    let value = parse_value(&mut chars)?;
    Some(value)
}

fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
}

fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Value> {
    skip_whitespace(chars);
    match chars.peek()? {
        '{' => parse_object(chars),
        '[' => parse_array(chars),
        '"' => parse_string(chars).map(Value::String),
        't' => parse_literal(chars, "true", Value::Bool(true)),
        'f' => parse_literal(chars, "false", Value::Bool(false)),
        'n' => parse_literal(chars, "null", Value::Null),
        _ => parse_number(chars),
    }
}

fn parse_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    literal: &str,
    value: Value,
) -> Option<Value> {
    for expected in literal.chars() {
        if chars.next()? != expected {
            return None;
        }
    }
    Some(value)
}

fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Value> {
    let mut text = String::new();
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '-'|'+'|'.'|'e'|'E'))
    {
        text.push(chars.next()?);
    }
    text.parse::<f64>().ok().map(Value::Number)
}

fn parse_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    loop {
        let c = chars.next()?;
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = (0..4).map(|_| chars.next()).collect::<Option<String>>()?;
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
}

fn parse_array(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Value> {
    chars.next(); // '['
    let mut items = Vec::new();
    skip_whitespace(chars);
    if chars.peek() == Some(&']') {
        chars.next();
        return Some(Value::Array(items));
    }
    loop {
        items.push(parse_value(chars)?);
        skip_whitespace(chars);
        match chars.next()? {
            ',' => continue,
            ']' => return Some(Value::Array(items)),
            _ => return None,
        }
    }
}

fn parse_object(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Value> {
    chars.next(); // '{'
    let mut pairs = Vec::new();
    skip_whitespace(chars);
    if chars.peek() == Some(&'}') {
        chars.next();
        return Some(Value::Object(pairs));
    }
    loop {
        skip_whitespace(chars);
        let key = parse_string(chars)?;
        skip_whitespace(chars);
        if chars.next()? != ':' {
            return None;
        }
        let value = parse_value(chars)?;
        pairs.push((key, value));
        skip_whitespace(chars);
        match chars.next()? {
            ',' => continue,
            '}' => return Some(Value::Object(pairs)),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_flat_object() {
        let value = Value::object([("a", Value::Number(1.0)), ("b", Value::Bool(true))]);
        let text = value.to_json_string();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.get("a").unwrap().as_f64(), Some(1.0));
        assert_eq!(parsed.get("b").unwrap(), &Value::Bool(true));
    }

    #[test]
    fn parses_nested_arrays_and_objects() {
        let text = r#"{"items":[{"n":1},{"n":2}],"ok":null}"#;
        let value = parse(text).unwrap();
        let items = value.get("items").unwrap().as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].get("n").unwrap().as_f64(), Some(2.0));
        assert_eq!(value.get("ok").unwrap(), &Value::Null);
    }

    #[test]
    fn parses_escaped_strings() {
        let value = parse(r#"{"s":"line1\nline2 \"quoted\""}"#).unwrap();
        assert_eq!(value.get("s").unwrap().as_str().unwrap(), "line1\nline2 \"quoted\"");
    }

    #[test]
    fn a_realistic_publish_diagnostics_notification_parses() {
        let text = r#"{
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///C:/proj/src/main.rs",
                "diagnostics": [
                    {"range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                     "severity": 1, "message": "unused variable"}
                ]
            }
        }"#;
        let value = parse(text).unwrap();
        assert_eq!(value.get("method").unwrap().as_str(), Some("textDocument/publishDiagnostics"));
        let params = value.get("params").unwrap();
        assert_eq!(params.get("uri").unwrap().as_str(), Some("file:///C:/proj/src/main.rs"));
        let diagnostics = params.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(diagnostics.len(), 1);
        let range = diagnostics[0].get("range").unwrap();
        assert_eq!(range.get("start").unwrap().get("line").unwrap().as_usize(), Some(3));
    }

    #[test]
    fn escaping_round_trips_through_the_writer() {
        let value = Value::String("a\"b\\c\nd".to_string());
        let text = value.to_json_string();
        assert_eq!(parse(&text).unwrap(), value);
    }
}
