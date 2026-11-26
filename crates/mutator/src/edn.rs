//! Read and write the EDN subset uml-viewer loads with clojure.edn.

use std::collections::BTreeSet;
use std::fmt::Write;

/// Value representation for the EDN subset used by `.metrics/mutate/*.edn`.
#[derive(Debug, Clone, PartialEq)]
pub enum EdnValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Keyword(String),
    Vec(Vec<EdnValue>),
    Set(Vec<EdnValue>),
    Map(Vec<(EdnValue, EdnValue)>),
}

impl EdnValue {
    /// Return string slice for `Str` or `Keyword`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            EdnValue::Str(s) | EdnValue::Keyword(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Return integer value if `Int`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            EdnValue::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Return slice of items if `Vec`.
    pub fn as_vec(&self) -> Option<&[EdnValue]> {
        match self {
            EdnValue::Vec(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// Return map entries if `Map`.
    pub fn as_map(&self) -> Option<&[(EdnValue, EdnValue)]> {
        match self {
            EdnValue::Map(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// Look up a string key in a `Map`.
    pub fn get(&self, key: &str) -> Option<&EdnValue> {
        let entries = self.as_map()?;
        entries
            .iter()
            .rfind(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }
}

/// Construct an EDN keyword value (`:name`).
pub fn keyword(name: impl Into<String>) -> EdnValue {
    EdnValue::Keyword(name.into())
}

fn escape_edn_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            other => out.push(other),
        }
    }
    out
}

fn is_keyword_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let valid_first = first.is_ascii_alphabetic() || matches!(first, '*' | '+' | '!' | '_' | '?');
    if !valid_first {
        return false;
    }
    chars.all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '*' | '+' | '!' | '_' | '?' | '-' | '.' | '/')
    })
}

fn render_float(value: f64) -> String {
    let mut text = format!("{value:.4}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if !text.contains('.') {
        text.push_str(".0");
    }
    text
}

fn render_key(key: &EdnValue) -> String {
    if let EdnValue::Str(s) = key
        && is_keyword_key(s)
    {
        return format!(":{s}");
    }
    dumps(key)
}

/// Render a value as EDN.
///
/// String map keys that are plain identifiers become keywords (`:key`).
pub fn dumps(value: &EdnValue) -> String {
    let mut out = String::new();
    render_into(value, &mut out);
    out
}

fn render_into(value: &EdnValue, out: &mut String) {
    match value {
        EdnValue::Nil => out.push_str("nil"),
        EdnValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        EdnValue::Int(n) => {
            let _ = write!(out, "{n}");
        }
        EdnValue::Float(f) => out.push_str(&render_float(*f)),
        EdnValue::Str(s) => {
            out.push('"');
            out.push_str(&escape_edn_str(s));
            out.push('"');
        }
        EdnValue::Keyword(k) => {
            out.push(':');
            out.push_str(k);
        }
        EdnValue::Vec(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(' ');
                }
                render_into(item, out);
            }
            out.push(']');
        }
        EdnValue::Set(items) => {
            out.push_str("#{");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(' ');
                }
                render_into(item, out);
            }
            out.push('}');
        }
        EdnValue::Map(entries) => {
            out.push('{');
            for (idx, (k, v)) in entries.iter().enumerate() {
                if idx > 0 {
                    out.push(' ');
                }
                out.push_str(&render_key(k));
                out.push(' ');
                render_into(v, out);
            }
            out.push('}');
        }
    }
}

/// Parse one EDN value. Keywords become plain strings without the leading colon.
pub fn loads(text: &str) -> Result<EdnValue, String> {
    let mut reader = Reader::new(text);
    let parsed = reader.parse()?;
    Ok(normalize_loaded(parsed))
}

#[derive(Debug, Clone)]
enum ParsedValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Vec(Vec<ParsedValue>),
    Set(Vec<ParsedValue>),
    Map(Vec<(ParsedValue, ParsedValue)>),
}

struct Reader<'a> {
    bytes: &'a [u8],
    text: &'a str,
    index: usize,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            text,
            index: 0,
        }
    }

    fn parse(&mut self) -> Result<ParsedValue, String> {
        self.skip();
        if self.index >= self.bytes.len() {
            return Err("empty EDN".to_string());
        }
        let value = self.value()?;
        self.skip();
        if self.index < self.bytes.len() {
            return Err(format!("trailing EDN at {}", self.char_index(self.index)));
        }
        Ok(value)
    }

    fn char_index(&self, byte_idx: usize) -> usize {
        self.text[..byte_idx].chars().count()
    }

    fn peek_char(&self) -> Result<char, String> {
        self.text[self.index..]
            .chars()
            .next()
            .ok_or_else(|| "unexpected end of EDN".to_string())
    }

    fn next_char(&mut self) -> Result<char, String> {
        let ch = self.peek_char()?;
        self.index += ch.len_utf8();
        Ok(ch)
    }

    fn skip(&mut self) {
        while let Some(ch) = self.text[self.index..].chars().next() {
            if ch.is_whitespace() || ch == ',' {
                self.index += ch.len_utf8();
                continue;
            }
            if ch == ';' {
                while let Some(c) = self.text[self.index..].chars().next() {
                    if c == '\n' {
                        break;
                    }
                    self.index += c.len_utf8();
                }
                continue;
            }
            break;
        }
    }

    fn value(&mut self) -> Result<ParsedValue, String> {
        self.skip();
        let ch = self.peek_char()?;
        match ch {
            '"' => self.string().map(ParsedValue::Str),
            ':' => self.keyword().map(ParsedValue::Str),
            '{' => self.map_collection(),
            '[' => self.seq_collection(']').map(ParsedValue::Vec),
            '(' => self.seq_collection(')').map(ParsedValue::Vec),
            '#' => {
                self.index += 1;
                if self.peek_char()? == '{' {
                    self.seq_collection('}').map(ParsedValue::Set)
                } else {
                    Err("unsupported EDN dispatch".to_string())
                }
            }
            _ => self.atom(),
        }
    }

    fn seq_collection(&mut self, closing: char) -> Result<Vec<ParsedValue>, String> {
        self.index += 1;
        let mut values = Vec::new();
        loop {
            self.skip();
            if self.peek_char()? == closing {
                self.index += 1;
                return Ok(values);
            }
            values.push(self.value()?);
        }
    }

    fn map_collection(&mut self) -> Result<ParsedValue, String> {
        self.index += 1;
        let mut entries = Vec::new();
        loop {
            self.skip();
            if self.peek_char()? == '}' {
                self.index += 1;
                return Ok(ParsedValue::Map(entries));
            }
            let key = self.value()?;
            self.skip();
            let val = self.value()?;
            entries.push((key, val));
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.index += 1;
        let mut out = String::new();
        while self.index < self.bytes.len() {
            let ch = self.next_char()?;
            if ch == '"' {
                return Ok(out);
            }
            if ch == '\\' {
                if self.index >= self.bytes.len() {
                    return Err("unterminated string escape".to_string());
                }
                let escaped = self.next_char()?;
                match escaped {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    other => out.push(other),
                }
                continue;
            }
            out.push(ch);
        }
        Err("unterminated string".to_string())
    }

    fn keyword(&mut self) -> Result<String, String> {
        self.index += 1;
        let start = self.index;
        while let Some(ch) = self.text[self.index..].chars().next() {
            if is_symbol_char(ch) {
                self.index += ch.len_utf8();
            } else {
                break;
            }
        }
        if self.index == start {
            return Err("empty keyword".to_string());
        }
        Ok(self.text[start..self.index].to_string())
    }

    fn atom(&mut self) -> Result<ParsedValue, String> {
        let start = self.index;
        while let Some(ch) = self.text[self.index..].chars().next() {
            if is_symbol_char(ch) {
                self.index += ch.len_utf8();
            } else {
                break;
            }
        }
        let token = &self.text[start..self.index];
        if token.is_empty() {
            let ch = self.peek_char()?;
            return Err(format!("unexpected character {ch:?}"));
        }
        match token {
            "nil" => Ok(ParsedValue::Nil),
            "true" => Ok(ParsedValue::Bool(true)),
            "false" => Ok(ParsedValue::Bool(false)),
            _ if is_int_token(token) => token
                .parse::<i64>()
                .map(ParsedValue::Int)
                .map_err(|_| format!("integer out of range: {token}")),
            _ if is_float_token(token) => token
                .parse::<f64>()
                .map(ParsedValue::Float)
                .map_err(|_| format!("invalid float: {token}")),
            _ => Ok(ParsedValue::Str(token.to_string())),
        }
    }
}

fn is_symbol_char(ch: char) -> bool {
    !matches!(
        ch,
        ' ' | '\t' | '\r' | '\n' | ',' | '{' | '}' | '[' | ']' | '(' | ')' | '"' | '\\' | ';'
    )
}

fn is_int_token(token: &str) -> bool {
    let digits = token.strip_prefix('-').unwrap_or(token);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_float_token(token: &str) -> bool {
    let body = token.strip_prefix('-').unwrap_or(token);
    let Some((whole, frac)) = body.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && !frac.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

fn normalize_loaded(value: ParsedValue) -> EdnValue {
    match value {
        ParsedValue::Nil => EdnValue::Nil,
        ParsedValue::Bool(b) => EdnValue::Bool(b),
        ParsedValue::Int(n) => EdnValue::Int(n),
        ParsedValue::Float(f) => EdnValue::Float(f),
        ParsedValue::Str(s) => EdnValue::Str(s),
        ParsedValue::Vec(items) => EdnValue::Vec(items.into_iter().map(normalize_loaded).collect()),
        ParsedValue::Set(items) => {
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for item in items.into_iter().map(normalize_loaded) {
                let rendered = dumps(&item);
                if seen.insert(rendered) {
                    out.push(item);
                }
            }
            EdnValue::Set(out)
        }
        ParsedValue::Map(entries) => {
            // Preserve last value per key while keeping insertion order of keys.
            let mut out: Vec<(EdnValue, EdnValue)> = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                let key = normalize_key(normalize_loaded(k));
                let val = normalize_loaded(v);
                if let Some(pos) = out.iter().position(|(existing, _)| existing == &key) {
                    out[pos].1 = val;
                } else {
                    out.push((key, val));
                }
            }
            EdnValue::Map(out)
        }
    }
}

fn normalize_key(key: EdnValue) -> EdnValue {
    match key {
        EdnValue::Nil
        | EdnValue::Bool(_)
        | EdnValue::Int(_)
        | EdnValue::Float(_)
        | EdnValue::Str(_) => key,
        other => EdnValue::Str(dumps(&other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_matches_snapshot_shape() {
        let payload = EdnValue::Map(vec![
            (EdnValue::Str("version".into()), EdnValue::Int(1)),
            (
                EdnValue::Str("tested-at".into()),
                EdnValue::Str("2026-09-30T12:00:00-05:00".into()),
            ),
            (
                EdnValue::Str("source".into()),
                EdnValue::Str("src/demo/say \"hi\".py".into()),
            ),
            (
                EdnValue::Str("namespace".into()),
                EdnValue::Str("demo.Board".into()),
            ),
            (
                EdnValue::Str("outcomes".into()),
                EdnValue::Map(vec![
                    (
                        EdnValue::Str(
                            "[\"src/a.py\",\"demo.Board\",\"defn/place\",1,2,\">\",\">=\"]".into(),
                        ),
                        keyword("killed"),
                    ),
                    (
                        EdnValue::Str(
                            "[\"src/a.py\",\"demo.Board\",\"defn-/hide\",3,4,\"true\",\"false\"]"
                                .into(),
                        ),
                        keyword("survived"),
                    ),
                ]),
            ),
            (
                EdnValue::Str("forms".into()),
                EdnValue::Vec(vec![EdnValue::Map(vec![
                    (
                        EdnValue::Str("id".into()),
                        EdnValue::Str("defn/place".into()),
                    ),
                    (EdnValue::Str("kind".into()), EdnValue::Str("defn".into())),
                    (
                        EdnValue::Str("file".into()),
                        EdnValue::Str("src/demo/Board.java".into()),
                    ),
                    (EdnValue::Str("line".into()), EdnValue::Int(4)),
                    (EdnValue::Str("end-line".into()), EdnValue::Int(9)),
                    (EdnValue::Str("hash".into()), EdnValue::Str("abc".into())),
                    (EdnValue::Str("killed".into()), EdnValue::Int(1)),
                    (EdnValue::Str("survived".into()), EdnValue::Int(0)),
                    (EdnValue::Str("uncovered".into()), EdnValue::Int(2)),
                    (EdnValue::Str("sites".into()), EdnValue::Int(3)),
                ])]),
            ),
        ]);
        let text = dumps(&payload);
        assert!(text.starts_with('{'));
        assert!(text.contains(":namespace"));
        assert!(text.contains(":killed"));

        let parsed = loads(&text).unwrap();
        assert_eq!(
            parsed.get("namespace").and_then(EdnValue::as_str),
            Some("demo.Board")
        );
        let forms = parsed.get("forms").and_then(EdnValue::as_vec).unwrap();
        assert_eq!(
            forms[0].get("id").and_then(EdnValue::as_str),
            Some("defn/place")
        );
        assert_eq!(forms[0].get("end-line").and_then(EdnValue::as_i64), Some(9));
        assert_eq!(forms[0].get("killed").and_then(EdnValue::as_i64), Some(1));
    }

    #[test]
    fn empty_collections_nil_and_errors() {
        let parsed = loads("{:forms [] :outcomes {} :note nil}").unwrap();
        assert_eq!(parsed.get("forms"), Some(&EdnValue::Vec(vec![])));
        assert_eq!(parsed.get("outcomes"), Some(&EdnValue::Map(vec![])));
        assert_eq!(parsed.get("note"), Some(&EdnValue::Nil));

        assert_eq!(loads(""), Err("empty EDN".into()));
        assert_eq!(loads("; hi"), Err("empty EDN".into()));
        assert_eq!(loads("{"), Err("unexpected end of EDN".into()));
        assert_eq!(loads("\"hi"), Err("unterminated string".into()));
        assert_eq!(loads("\"\\"), Err("unterminated string escape".into()));
        assert_eq!(loads(":"), Err("empty keyword".into()));
        assert_eq!(loads("; kept\n12"), Ok(EdnValue::Int(12)));
        assert_eq!(
            loads("#{1 2}"),
            Ok(EdnValue::Set(vec![EdnValue::Int(1), EdnValue::Int(2)]))
        );
        assert_eq!(
            loads("{nil 1}"),
            Ok(EdnValue::Map(vec![(EdnValue::Nil, EdnValue::Int(1))]))
        );
        assert_eq!(loads("true"), Ok(EdnValue::Bool(true)));
        assert_eq!(loads("false"), Ok(EdnValue::Bool(false)));
        assert_eq!(loads("-3"), Ok(EdnValue::Int(-3)));
        assert_eq!(loads("1.50"), Ok(EdnValue::Float(1.5)));
        assert_eq!(loads("foo"), Ok(EdnValue::Str("foo".into())));
    }
}
