//! Read Clojure source into forms and normalize them matching dry4clj.

use super::shape::NormNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollKind {
    List,
    Vector,
    Map,
    Set,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    Sym {
        name: String,
        line: usize,
    },
    Kw {
        name: String,
        line: usize,
    },
    Lit {
        line: usize,
    },
    Coll {
        kind: CollKind,
        line: usize,
        items: Vec<Form>,
        pairs: Vec<(Form, Form)>,
    },
}

impl Drop for Form {
    fn drop(&mut self) {
        let Form::Coll { items, pairs, .. } = self else {
            return;
        };
        let mut stack: Vec<Form> = std::mem::take(items);
        for (k, v) in std::mem::take(pairs) {
            stack.push(k);
            stack.push(v);
        }
        while let Some(mut form) = stack.pop() {
            if let Form::Coll {
                items: inner_items,
                pairs: inner_pairs,
                ..
            } = &mut form
            {
                stack.append(inner_items);
                for (k, v) in inner_pairs.drain(..) {
                    stack.push(k);
                    stack.push(v);
                }
            }
        }
    }
}

impl Form {
    pub fn line(&self) -> usize {
        match self {
            Form::Sym { line, .. }
            | Form::Kw { line, .. }
            | Form::Lit { line }
            | Form::Coll { line, .. } => *line,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadError {
    line: usize,
    message: String,
}

enum Produced {
    Need,
    None,
    Splice(Vec<Form>),
    One(Form),
}

enum Frame {
    Coll {
        end: char,
        kind: CollKind,
        line: usize,
        items: Vec<Form>,
        fn_star: bool,
    },
    Wrap {
        name: &'static str,
        line: usize,
    },
    Discard,
    Meta,
    Lit {
        line: usize,
    },
    Cond {
        splicing: bool,
    },
}

fn is_ws(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | ',')
}

fn is_terminator(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | ';'
            | '\''
            | '@'
            | '^'
            | '`'
            | '~'
            | '\\'
            | ','
            | '#'
            | ':'
    )
}

fn coll_kind_name(kind: CollKind) -> &'static str {
    match kind {
        CollKind::List => "list",
        CollKind::Vector => "vector",
        CollKind::Map => "map",
        CollKind::Set => "set",
        CollKind::Unknown => "unknown",
    }
}

fn is_clojure_number(token: &str) -> bool {
    let bytes = token.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let body = &bytes[i..];
    if body.is_empty() {
        return false;
    }
    let body = if matches!(body.last(), Some(b'M' | b'N')) {
        &body[..body.len() - 1]
    } else {
        body
    };
    if body.is_empty() {
        return false;
    }
    // 0[xX][0-9A-Fa-f]+
    if body.len() > 2 && body[0] == b'0' && (body[1] == b'x' || body[1] == b'X') {
        return body[2..].iter().all(u8::is_ascii_hexdigit);
    }
    // \d+/\d+
    if let Some(slash) = body.iter().position(|&b| b == b'/') {
        let num = &body[..slash];
        let den = &body[slash + 1..];
        return !num.is_empty()
            && !den.is_empty()
            && num.iter().all(u8::is_ascii_digit)
            && den.iter().all(u8::is_ascii_digit);
    }
    // (\d+\.\d+ | \.\d+ | \d+)(?:[eE][+-]?\d+)?
    let (mantissa, exp) = match body.iter().position(|&b| b == b'e' || b == b'E') {
        Some(pos) => (&body[..pos], Some(&body[pos + 1..])),
        None => (body, None),
    };
    if let Some(exp_digits) = exp {
        let exp_digits = if exp_digits.first().is_some_and(|&b| b == b'+' || b == b'-') {
            &exp_digits[1..]
        } else {
            exp_digits
        };
        if exp_digits.is_empty() || !exp_digits.iter().all(u8::is_ascii_digit) {
            return false;
        }
    }
    match mantissa.iter().position(|&b| b == b'.') {
        Some(dot) => {
            let left = &mantissa[..dot];
            let right = &mantissa[dot + 1..];
            !right.is_empty()
                && left.iter().all(u8::is_ascii_digit)
                && right.iter().all(u8::is_ascii_digit)
        }
        None => !mantissa.is_empty() && mantissa.iter().all(u8::is_ascii_digit),
    }
}

fn close_coll(
    kind: CollKind,
    line: usize,
    items: Vec<Form>,
    fn_star: bool,
) -> Result<Form, ReadError> {
    if fn_star {
        let mut wrapped = Vec::with_capacity(items.len() + 1);
        wrapped.push(Form::Sym {
            name: "fn*".to_string(),
            line,
        });
        wrapped.extend(items);
        return Ok(Form::Coll {
            kind: CollKind::List,
            line,
            items: wrapped,
            pairs: Vec::new(),
        });
    }
    if kind == CollKind::Map {
        if !items.len().is_multiple_of(2) {
            return Err(ReadError {
                line,
                message: "map literal has an odd number of forms".to_string(),
            });
        }
        let mut pairs = Vec::with_capacity(items.len() / 2);
        let mut iter = items.into_iter();
        while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
            pairs.push((k, v));
        }
        return Ok(Form::Coll {
            kind: CollKind::Map,
            line,
            items: Vec::new(),
            pairs,
        });
    }
    Ok(Form::Coll {
        kind,
        line,
        items,
        pairs: Vec::new(),
    })
}

fn chosen_branch(items: Vec<Form>) -> Option<Form> {
    let mut iter = items.into_iter();
    while let (Some(feature), Some(expr)) = (iter.next(), iter.next()) {
        if let Form::Kw { name, .. } = &feature
            && (name == "clj" || name == "default")
        {
            return Some(expr);
        }
    }
    None
}

struct Reader {
    chars: Vec<char>,
    i: usize,
    line: usize,
}

impl Reader {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            i: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn eof(&self) -> bool {
        self.i >= self.chars.len()
    }

    fn get(&mut self) -> Option<char> {
        let ch = self.chars.get(self.i).copied()?;
        self.i += 1;
        if ch == '\n' {
            self.line += 1;
        }
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if is_ws(ch) {
                self.get();
                continue;
            }
            if ch == ';' {
                while let Some(comment_ch) = self.peek() {
                    if comment_ch == '\n' {
                        break;
                    }
                    self.get();
                }
                continue;
            }
            break;
        }
    }

    fn read_token(&mut self) -> Result<String, ReadError> {
        let start = self.i;
        let line = self.line;
        while let Some(ch) = self.peek() {
            if is_ws(ch) || is_terminator(ch) {
                break;
            }
            self.get();
        }
        if self.i == start {
            return Err(ReadError {
                line,
                message: "expected a symbol".to_string(),
            });
        }
        Ok(self.chars[start..self.i].iter().collect())
    }

    fn read_string(&mut self) -> Result<Form, ReadError> {
        let line = self.line;
        self.get();
        while let Some(ch) = self.get() {
            if ch == '\\' {
                if self.eof() {
                    break;
                }
                self.get();
                continue;
            }
            if ch == '"' {
                return Ok(Form::Lit { line });
            }
        }
        Err(ReadError {
            line,
            message: "unterminated string".to_string(),
        })
    }

    fn read_char(&mut self) -> Result<Form, ReadError> {
        let line = self.line;
        self.get();
        if self.eof() {
            return Err(ReadError {
                line,
                message: "incomplete character".to_string(),
            });
        }
        self.get();
        while let Some(ch) = self.peek() {
            if is_ws(ch) || is_terminator(ch) {
                break;
            }
            self.get();
        }
        Ok(Form::Lit { line })
    }

    fn read_keyword(&mut self) -> Result<Form, ReadError> {
        let line = self.line;
        self.get();
        if self.peek() == Some(':') {
            self.get();
        }
        match self.peek() {
            None => Ok(Form::Kw {
                name: String::new(),
                line,
            }),
            Some(ch) if is_ws(ch) || is_terminator(ch) => Ok(Form::Kw {
                name: String::new(),
                line,
            }),
            Some(_) => Ok(Form::Kw {
                name: self.read_token()?,
                line,
            }),
        }
    }

    fn read_map_qualifier(&mut self) -> Result<(), ReadError> {
        match self.peek() {
            Some(':') => {
                self.get();
                Ok(())
            }
            None | Some('{') => Ok(()),
            Some(ch) if is_ws(ch) => Ok(()),
            Some(_) => {
                self.read_token()?;
                Ok(())
            }
        }
    }

    fn open_dispatch(
        &mut self,
        stack: &mut Vec<Frame>,
        line: usize,
    ) -> Result<Produced, ReadError> {
        self.get();
        let Some(ch) = self.peek() else {
            return Err(ReadError {
                line,
                message: "incomplete dispatch".to_string(),
            });
        };
        match ch {
            '_' => {
                self.get();
                stack.push(Frame::Discard);
                Ok(Produced::Need)
            }
            '{' => {
                let brace = self.line;
                self.get();
                stack.push(Frame::Coll {
                    end: '}',
                    kind: CollKind::Set,
                    line: brace,
                    items: Vec::new(),
                    fn_star: false,
                });
                Ok(Produced::Need)
            }
            '(' => {
                self.get();
                stack.push(Frame::Coll {
                    end: ')',
                    kind: CollKind::List,
                    line,
                    items: Vec::new(),
                    fn_star: true,
                });
                Ok(Produced::Need)
            }
            '"' => Ok(Produced::One(self.read_string()?)),
            '?' => {
                self.get();
                let splicing = self.peek() == Some('@');
                if splicing {
                    self.get();
                }
                stack.push(Frame::Cond { splicing });
                Ok(Produced::Need)
            }
            ':' => {
                self.get();
                self.read_map_qualifier()?;
                Ok(Produced::Need)
            }
            _ => {
                self.read_token()?;
                stack.push(Frame::Lit { line });
                Ok(Produced::Need)
            }
        }
    }

    fn open(&mut self, stack: &mut Vec<Frame>) -> Result<Produced, ReadError> {
        let ch = self.peek().expect("not eof");
        let line = self.line;
        match ch {
            '(' | '[' | '{' => {
                self.get();
                let (end, kind) = match ch {
                    '(' => (')', CollKind::List),
                    '[' => (']', CollKind::Vector),
                    _ => ('}', CollKind::Map),
                };
                stack.push(Frame::Coll {
                    end,
                    kind,
                    line,
                    items: Vec::new(),
                    fn_star: false,
                });
                Ok(Produced::Need)
            }
            '"' => Ok(Produced::One(self.read_string()?)),
            '\\' => Ok(Produced::One(self.read_char()?)),
            ':' => Ok(Produced::One(self.read_keyword()?)),
            '\'' | '@' | '`' => {
                self.get();
                let name = match ch {
                    '\'' => "quote",
                    '@' => "deref",
                    _ => "syntax-quote",
                };
                stack.push(Frame::Wrap { name, line });
                Ok(Produced::Need)
            }
            '~' => {
                self.get();
                let name = if self.peek() == Some('@') {
                    self.get();
                    "unquote-splicing"
                } else {
                    "unquote"
                };
                stack.push(Frame::Wrap { name, line });
                Ok(Produced::Need)
            }
            '^' => {
                self.get();
                stack.push(Frame::Meta);
                Ok(Produced::Need)
            }
            '#' => self.open_dispatch(stack, line),
            _ => {
                let token = self.read_token()?;
                if is_clojure_number(&token) {
                    Ok(Produced::One(Form::Lit { line }))
                } else {
                    Ok(Produced::One(Form::Sym { name: token, line }))
                }
            }
        }
    }

    fn next_step(&mut self, stack: &mut Vec<Frame>) -> Result<Produced, ReadError> {
        self.skip_ws();
        if let Some(Frame::Coll {
            end, kind, line, ..
        }) = stack.last()
        {
            if self.eof() {
                return Err(ReadError {
                    line: *line,
                    message: format!("unterminated {}", coll_kind_name(*kind)),
                });
            }
            if self.peek() == Some(*end) {
                self.get();
                let Some(Frame::Coll {
                    kind,
                    line,
                    items,
                    fn_star,
                    ..
                }) = stack.pop()
                else {
                    unreachable!();
                };
                return Ok(Produced::One(close_coll(kind, line, items, fn_star)?));
            }
        }
        if self.eof() {
            return Err(ReadError {
                line: self.line,
                message: "unexpected end of file".to_string(),
            });
        }
        self.open(stack)
    }

    fn finish_cond(&self, splicing: bool, mut produced: Produced) -> Result<Produced, ReadError> {
        let Produced::One(Form::Coll {
            kind: CollKind::List,
            items,
            ..
        }) = &mut produced
        else {
            return Err(ReadError {
                line: self.line,
                message: "reader conditional body must be a list".to_string(),
            });
        };
        let items = std::mem::take(items);
        let Some(mut chosen) = chosen_branch(items) else {
            return Ok(Produced::None);
        };
        if !splicing {
            return Ok(Produced::One(chosen));
        }
        match &mut chosen {
            Form::Coll {
                kind: CollKind::List | CollKind::Vector,
                items,
                ..
            } => Ok(Produced::Splice(std::mem::take(items))),
            _ => Err(ReadError {
                line: self.line,
                message: "splicing reader conditional needs a collection".to_string(),
            }),
        }
    }

    fn deliver(&self, stack: &mut Vec<Frame>, produced: Produced) -> Result<Produced, ReadError> {
        if let Some(Frame::Coll { items, .. }) = stack.last_mut() {
            match produced {
                Produced::None => {}
                Produced::Splice(more) => items.extend(more),
                Produced::One(form) => items.push(form),
                Produced::Need => unreachable!(),
            }
            return Ok(Produced::Need);
        }
        let frame = stack.pop().expect("stack non-empty");
        match frame {
            Frame::Coll { .. } => unreachable!(),
            Frame::Wrap { name, line } => {
                let mut items = vec![Form::Sym {
                    name: name.to_string(),
                    line,
                }];
                if let Produced::One(form) = produced {
                    items.push(form);
                }
                Ok(Produced::One(Form::Coll {
                    kind: CollKind::List,
                    line,
                    items,
                    pairs: Vec::new(),
                }))
            }
            Frame::Discard => Ok(Produced::None),
            Frame::Meta => Ok(Produced::Need),
            Frame::Lit { line } => Ok(Produced::One(Form::Lit { line })),
            Frame::Cond { splicing } => self.finish_cond(splicing, produced),
        }
    }

    fn read_form(&mut self) -> Result<Option<Form>, ReadError> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut produced = Produced::Need;
        loop {
            if matches!(produced, Produced::Need) {
                produced = self.next_step(&mut stack)?;
                continue;
            }
            if stack.is_empty() {
                return Ok(match produced {
                    Produced::One(form) => Some(form),
                    Produced::None | Produced::Splice(_) => None,
                    Produced::Need => unreachable!(),
                });
            }
            produced = self.deliver(&mut stack, produced)?;
        }
    }
}

/// Top-level forms, plus a warning if a later form could not be read.
pub fn read_source(text: &str) -> (Vec<Form>, Option<String>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = Reader::new(text);
    if text.starts_with("#!") {
        while let Some(ch) = reader.peek() {
            if ch == '\n' {
                break;
            }
            reader.get();
        }
    }
    let mut forms = Vec::new();
    loop {
        reader.skip_ws();
        if reader.eof() {
            return (forms, None);
        }
        match reader.read_form() {
            Ok(Some(form)) => forms.push(form),
            Ok(None) => {}
            Err(err) => return (forms, Some(format!("{}: {}", err.line, err.message))),
        }
    }
}

pub fn max_line(form: &Form) -> usize {
    let mut best = form.line().max(1);
    let mut stack = vec![form];
    while let Some(current) = stack.pop() {
        best = best.max(current.line().max(1));
        if let Form::Coll { items, pairs, .. } = current {
            stack.extend(items.iter());
            for (k, v) in pairs {
                stack.push(k);
                stack.push(v);
            }
        }
    }
    best
}

fn normalize_atom(form: &Form, head: bool) -> NormNode {
    match form {
        Form::Sym { name, .. } => {
            if head {
                NormNode::List(vec![
                    NormNode::Keyword("symbol".into()),
                    NormNode::Text(name.clone()),
                ])
            } else {
                NormNode::Keyword("symbol".into())
            }
        }
        Form::Kw { .. } => NormNode::Keyword("keyword".into()),
        Form::Lit { .. } => NormNode::Keyword("literal".into()),
        Form::Coll { .. } => unreachable!(),
    }
}

enum NormStep<'a> {
    Enter(&'a Form, bool),
    AssembleList(usize),
    AssembleFlat(CollKind, usize),
    AssembleMap(usize),
}

/// Normalize a Clojure form matching dry4clj's `normalize-form`. Iterative for deep trees.
pub fn normalize(form: &Form) -> NormNode {
    let mut values: Vec<NormNode> = Vec::new();
    let mut work = vec![NormStep::Enter(form, false)];

    while let Some(step) = work.pop() {
        match step {
            NormStep::Enter(current, is_head) => match current {
                Form::Coll {
                    kind, items, pairs, ..
                } => match kind {
                    CollKind::List => {
                        if items.is_empty() {
                            values.push(NormNode::List(vec![
                                NormNode::Keyword("list".into()),
                                NormNode::Keyword("literal".into()),
                            ]));
                        } else {
                            work.push(NormStep::AssembleList(items.len()));
                            for item in items[1..].iter().rev() {
                                work.push(NormStep::Enter(item, false));
                            }
                            work.push(NormStep::Enter(&items[0], true));
                        }
                    }
                    CollKind::Vector | CollKind::Set => {
                        work.push(NormStep::AssembleFlat(*kind, items.len()));
                        for item in items.iter().rev() {
                            work.push(NormStep::Enter(item, false));
                        }
                    }
                    CollKind::Map => {
                        work.push(NormStep::AssembleMap(pairs.len()));
                        for (k, v) in pairs.iter().rev() {
                            work.push(NormStep::Enter(v, false));
                            work.push(NormStep::Enter(k, false));
                        }
                    }
                    CollKind::Unknown => {
                        values.push(NormNode::List(vec![NormNode::Keyword("literal".into())]));
                    }
                },
                atom => values.push(normalize_atom(atom, is_head)),
            },
            NormStep::AssembleList(count) => {
                let start = values.len() - count;
                let mut out = Vec::with_capacity(count + 1);
                out.push(NormNode::Keyword("list".into()));
                out.extend(values.drain(start..));
                values.push(NormNode::List(out));
            }
            NormStep::AssembleFlat(kind, count) => {
                let start = values.len() - count;
                let mut out = Vec::with_capacity(count + 1);
                out.push(NormNode::Keyword(coll_kind_name(kind).into()));
                out.extend(values.drain(start..));
                values.push(NormNode::List(out));
            }
            NormStep::AssembleMap(pair_count) => {
                let start = values.len() - pair_count * 2;
                let mut out = Vec::with_capacity(pair_count + 1);
                out.push(NormNode::Keyword("map".into()));
                {
                    let mut iter = values.drain(start..);
                    while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                        out.push(NormNode::List(vec![k, v]));
                    }
                }
                values.push(NormNode::List(out));
            }
        }
    }

    values.pop().expect("normalized root")
}

/// True for a non-empty top-level list whose head is not `ns`.
pub fn is_candidate_form(form: &Form) -> bool {
    let Form::Coll {
        kind: CollKind::List,
        items,
        ..
    } = form
    else {
        return false;
    };
    let Some(first) = items.first() else {
        return false;
    };
    !matches!(first, Form::Sym { name, .. } if name == "ns")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::{fingerprints, jaccard, pr};

    #[test]
    fn reads_reader_conditionals_wrappers_and_errors() {
        let (forms, warning) = read_source("#?(:cljs (inc x) :default (dec x))\n");
        assert_eq!(warning, None);
        assert!(
            matches!(&forms[0], Form::Coll { items, .. } if matches!(&items[0], Form::Sym { name, .. } if name == "dec"))
        );

        let (forms, warning) = read_source("#?[:clj (inc x)]\n");
        assert!(forms.is_empty());
        assert_eq!(
            warning.as_deref(),
            Some("1: reader conditional body must be a list")
        );

        let (forms, warning) = read_source("(defn alpha []\n  [#?@(:clj [:a :b])])\n");
        assert_eq!(warning, None);
        assert_eq!(forms.len(), 1);

        let (forms, warning) = read_source("'#_x\n");
        assert_eq!(warning, None);
        assert!(matches!(&forms[0], Form::Coll { items, .. } if items.len() == 1));
    }

    #[test]
    fn normalizes_renamed_clojure_functions_to_identical_fingerprints() {
        let (left, w1) =
            read_source("(defn alpha [xs]\n  (let [ys (filter odd? xs)]\n    (map inc ys)))\n");
        let (right, w2) = read_source(
            "(defn beta [items]\n  (let [kept (filter even? items)]\n    (map dec kept)))\n",
        );
        assert!(w1.is_none() && w2.is_none());
        let fp_l = fingerprints(&normalize(&left[0]));
        let fp_r = fingerprints(&normalize(&right[0]));
        assert_eq!(jaccard(&fp_l, &fp_r), 1.0);
    }

    #[test]
    fn handles_deep_clojure_form_and_empty_list() {
        let (forms, warning) = read_source("(defn alpha [x]\n  ())\n");
        assert_eq!(warning, None);
        assert!(pr(&normalize(&forms[0])).contains("[:list :literal]"));

        let nested = format!(
            "(defn place [] {}x{})",
            "(inc ".repeat(1200),
            ")".repeat(1200)
        );
        let (deep, warning) = read_source(&nested);
        assert_eq!(warning, None);
        assert_eq!(deep.len(), 1);
        assert!(!fingerprints(&normalize(&deep[0])).is_empty());
    }

    #[test]
    fn covers_escaped_strings_bom_shebang_maps_sets_and_candidate_rules() {
        let (forms, warning) = read_source("(defn alpha [x]\n  (str \"a\\\\nb\" \\newline \\a))\n");
        assert_eq!(warning, None);
        assert_eq!(forms.len(), 1);

        let (_, warning) = read_source("(defn alpha [x]\n  \"abc)\n");
        assert_eq!(warning.as_deref(), Some("2: unterminated string"));
        let (_, warning) = read_source("(defn alpha [x] \"abc\\");
        assert_eq!(warning.as_deref(), Some("1: unterminated string"));
        let (_, warning) = read_source("(defn alpha [x] \\");
        assert_eq!(warning.as_deref(), Some("1: incomplete character"));

        let (forms, warning) =
            read_source("\u{feff}#!/usr/bin/env bb\n(defn alpha [x]\n  (inc x))\n");
        assert_eq!(warning, None);
        assert_eq!(forms.len(), 1);

        let (forms, warning) = read_source(":\n");
        assert_eq!(warning, None);
        assert!(matches!(&forms[0], Form::Kw { name, .. } if name.is_empty()));

        for source in ["#:foo{:a 1}\n", "#::{:a 1}\n", "#:{:a 1}\n"] {
            let (forms, warning) = read_source(source);
            assert_eq!(warning, None, "{source}");
            assert!(matches!(
                &forms[0],
                Form::Coll {
                    kind: CollKind::Map,
                    ..
                }
            ));
        }

        let (left, _) = read_source("{foo 1}\n");
        let (right, _) = read_source("{bar 2}\n");
        assert_eq!(pr(&normalize(&left[0])), pr(&normalize(&right[0])));

        let (forms, warning) = read_source("#{foo bar}\n");
        assert_eq!(warning, None);
        let printed = pr(&normalize(&forms[0]));
        assert!(printed.starts_with("[:set "));
        assert!(!printed.contains("foo"));

        assert!(!is_candidate_form(&Form::Coll {
            kind: CollKind::List,
            line: 1,
            items: vec![Form::Sym {
                name: "ns".into(),
                line: 1
            }],
            pairs: vec![],
        }));
        assert_eq!(
            pr(&normalize(&Form::Coll {
                kind: CollKind::Unknown,
                line: 1,
                items: vec![],
                pairs: vec![],
            })),
            "[:literal]"
        );

        let (forms, warning) = read_source("#?(:clj (inc x) :extra)\n");
        assert_eq!(warning, None);
        assert_eq!(forms.len(), 1);
        let (forms, warning) = read_source("#?(:cljs (dec x) :extra)\n");
        assert_eq!(warning, None);
        assert!(forms.is_empty());

        let (forms, warning) = read_source("(defn alpha [x]\n  (inc x))\n#?@(:clj {:a 1})\n");
        assert_eq!(
            warning.as_deref(),
            Some("3: splicing reader conditional needs a collection")
        );
        assert_eq!(forms.len(), 1);

        let (forms, warning) = read_source("#!only");
        assert_eq!(warning, None);
        assert!(forms.is_empty());
    }

    #[test]
    fn invoice_and_receipt_score_matches_dry4clj_range() {
        let invoice = "(ns billing.invoice)

(defn invoice-summary [orders]
  (let [paid (filter paid? orders), domestic (filter domestic? paid)
        sorted (sort-by :date domestic), amounts (map :amount sorted)
        taxes (map tax amounts), ids (map :id sorted)
        customers (map :customer sorted), regions (group-by :region sorted)
        flagged (filter flagged? sorted)]
    {:count (count sorted)
     :first-id (first ids), :last-id (last ids)
     :customers (set customers), :regions (keys regions)
     :flagged (count flagged), :total (reduce + 0 amounts)
     :tax (reduce + 0 taxes)}))
";
        let receipt = "(ns billing.receipt)

(defn receipt-summary [rows]
  (let [closed (filter closed? rows), local (filter local? closed)
        ordered (sort-by :date local), amounts (map :amount ordered)
        taxable (filter taxable? ordered), taxes (map tax amounts)
        ids (map :id ordered), customers (map :customer ordered)
        regions (group-by :region ordered)
        flagged (filter flagged? ordered)]
    {:count (count ordered)
     :first-id (first ids), :last-id (last ids)
     :customers (set customers), :regions (keys regions)
     :flagged (count flagged), :total (reduce + 0 amounts)
     :tax (reduce + 0 taxes)}))
";
        let (mut entries_l, w1) = crate::extract::clojure_entries(invoice, "invoice.clj");
        let (entries_r, w2) = crate::extract::clojure_entries(receipt, "receipt.clj");
        assert!(w1.is_none() && w2.is_none());
        entries_l.extend(entries_r);
        let found = crate::scan::find_duplicates(&entries_l, 0.82);
        assert_eq!(found.len(), 1);
        assert!(found[0].score >= 0.85 && found[0].score < 1.0);
    }
}
