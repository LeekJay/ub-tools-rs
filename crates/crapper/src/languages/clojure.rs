use std::sync::OnceLock;

use regex::Regex;

use crate::model::Function;

fn decision_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\((if-not|if-let|if-some|when-not|when-let|when-some|when-first|if|when|and|or|loop|catch)[\s\)]",
        )
        .expect("valid clojure decision regex")
    })
}

fn cond_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\((some->>|some->|cond->>|cond->|cond|condp|case)[\s\)]")
            .expect("valid clojure cond regex")
    })
}

fn ns_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(\s*ns\s+([A-Za-z0-9*+!_?.\-/]+)").expect("valid ns regex"))
}

fn in_ns_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\(\s*in-ns\s+'([A-Za-z0-9*+!_?.\-/]+)\s*\)").expect("valid in-ns regex")
    })
}

fn in_ns_quote_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\(\s*in-ns\s+\(quote\s+([A-Za-z0-9*+!_?.\-/]+)\)\s*\)")
            .expect("valid in-ns quote regex")
    })
}

fn clojure_ext_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.(?:clj[cs]?|bb)$").expect("valid ext regex"))
}

fn form_skip(form_type: &str) -> usize {
    match form_type {
        "condp" => 2,
        "case" | "cond->" | "cond->>" | "some->" | "some->>" => 1,
        _ => 0,
    }
}

fn is_thread_form(form_type: &str) -> bool {
    matches!(form_type, "some->" | "some->>")
}

fn is_char_delimiter(ch: char) -> bool {
    matches!(ch, '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';' | ',')
}

fn char_at(source: &str, index: usize) -> Option<(char, usize)> {
    let ch = source.get(index..)?.chars().next()?;
    Some((ch, index + ch.len_utf8()))
}

fn blank_span(source: &str, start: usize, end: usize, out: &mut String) {
    for ch in source[start..end].chars() {
        out.push(if ch == '\n' { '\n' } else { ' ' });
    }
}

fn char_literal_end(source: &str, index: usize) -> usize {
    let first = index + 1;
    let Some((_, mut i)) = char_at(source, first) else {
        return source.len();
    };
    while let Some((ch, next)) = char_at(source, i) {
        if ch.is_whitespace() || is_char_delimiter(ch) {
            return i;
        }
        i = next;
    }
    source.len()
}

fn consume_string(source: &str, index: usize, mut line: usize) -> (usize, usize) {
    let mut i = index + 1;
    let mut escaped = false;
    while let Some((ch, next)) = char_at(source, i) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return (next, line);
        }
        if ch == '\n' {
            line += 1;
        }
        i = next;
    }
    (i, line)
}

fn consume_comment(source: &str, index: usize, line: usize) -> (usize, usize) {
    let mut i = index + 1;
    while let Some((ch, next)) = char_at(source, i) {
        if ch == '\n' {
            return (next, line + 1);
        }
        i = next;
    }
    (i, line)
}

fn advance_past_reader_token(source: &str, index: usize) -> Option<usize> {
    let (ch, _) = char_at(source, index)?;
    if ch == '"' {
        return Some(consume_string(source, index, 1).0);
    }
    if ch == '\\' {
        return Some(char_literal_end(source, index));
    }
    if ch != ';' {
        return None;
    }
    let mut i = index;
    while let Some((c, next)) = char_at(source, i) {
        if c == '\n' {
            break;
        }
        i = next;
    }
    Some(i)
}

fn balanced_end(source: &str, mut index: usize) -> usize {
    let opener = source[index..].chars().next().unwrap_or('(');
    let closer = match opener {
        '(' => ')',
        '[' => ']',
        _ => '}',
    };
    let mut depth = 0isize;
    while index < source.len() {
        if let Some(nxt) = advance_past_reader_token(source, index) {
            index = nxt;
            continue;
        }
        let Some((ch, next)) = char_at(source, index) else {
            break;
        };
        index = next;
        if ch == opener {
            depth += 1;
        } else if ch == closer {
            depth -= 1;
            if depth == 0 {
                return index;
            }
        }
    }
    index
}

fn skip_space_and_comment(source: &str, mut index: usize) -> usize {
    while let Some((ch, next)) = char_at(source, index) {
        if !ch.is_whitespace() {
            break;
        }
        index = next;
    }
    if matches!(char_at(source, index), Some((';', _))) {
        while let Some((ch, next)) = char_at(source, index) {
            if ch == '\n' {
                break;
            }
            index = next;
        }
    }
    index
}

fn skip_ignorable(source: &str, mut index: usize) -> usize {
    loop {
        let nxt = skip_space_and_comment(source, index);
        if nxt == index {
            return index;
        }
        index = nxt;
    }
}

fn end_of_atom(source: &str, mut index: usize) -> usize {
    while let Some((ch, next)) = char_at(source, index) {
        if ch.is_whitespace() || "()[]{}\";".contains(ch) {
            return index;
        }
        index = next;
    }
    index
}

fn end_of_one_form(source: &str, index: usize) -> usize {
    let Some((ch, _)) = char_at(source, index) else {
        return index;
    };
    if "([{".contains(ch) {
        return balanced_end(source, index);
    }
    if ch == '"' {
        return consume_string(source, index, 1).0;
    }
    if ch == '\\' {
        return char_literal_end(source, index);
    }
    if source[index..].starts_with("#_") {
        return form_end(source, index + 2);
    }
    end_of_atom(source, index)
}

fn form_end(source: &str, index: usize) -> usize {
    let index = skip_ignorable(source, index);
    if index >= source.len() {
        return index;
    }
    end_of_one_form(source, index)
}

pub fn without_strings_and_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < source.len() {
        if source[index..].starts_with("#_") {
            let end = form_end(source, index + 2);
            blank_span(source, index, end, &mut out);
            index = end;
            continue;
        }
        let Some((ch, next)) = char_at(source, index) else {
            break;
        };
        if ch == '"' {
            let end = consume_string(source, index, 1).0;
            blank_span(source, index, end, &mut out);
            index = end;
            continue;
        }
        if ch == '\\' {
            let end = char_literal_end(source, index);
            blank_span(source, index, end, &mut out);
            index = end;
            continue;
        }
        if ch == ';' {
            while let Some((c, n)) = char_at(source, index) {
                if c == '\n' {
                    break;
                }
                out.push(' ');
                index = n;
            }
            continue;
        }
        out.push(ch);
        index = next;
    }
    out
}

fn count_top_level_forms(text: &str, start: usize) -> usize {
    let mut depth = 1isize;
    let mut forms = 0usize;
    let mut in_form = false;
    let mut i = start;
    while i < text.len() && depth != 0 {
        let Some((ch, next)) = char_at(text, i) else {
            break;
        };
        if "({[".contains(ch) {
            if depth == 1 && !in_form {
                forms += 1;
            }
            depth += 1;
            in_form = true;
        } else if ")}]".contains(ch) {
            if depth == 1 {
                in_form = false;
            }
            depth -= 1;
        } else if ch.is_whitespace() {
            in_form = depth != 1;
        } else {
            if depth == 1 && !in_form {
                forms += 1;
            }
            in_form = true;
        }
        i = next;
    }
    forms
}

fn skip_to_body(text: &str, match_start: usize) -> usize {
    let mut i = match_start + 1;
    while let Some((ch, next)) = char_at(text, i) {
        if ch.is_whitespace() || ch == ')' {
            break;
        }
        i = next;
    }
    i
}

fn pairwise_clause_count(form_type: &str, remaining: isize) -> usize {
    let rem = remaining.max(0) as usize;
    let base = rem / 2;
    if form_type == "case" && rem % 2 == 1 {
        base + 1
    } else {
        base
    }
}

fn count_clauses(text: &str, form_type: &str, match_start: usize) -> usize {
    let body_start = skip_to_body(text, match_start);
    let total_forms = count_top_level_forms(text, body_start) as isize;
    let remaining = total_forms - form_skip(form_type) as isize;
    if is_thread_form(form_type) {
        remaining.max(0) as usize
    } else {
        pairwise_clause_count(form_type, remaining)
    }
}

fn count_cond_decisions(clean: &str) -> usize {
    cond_re()
        .captures_iter(clean)
        .map(|cap| {
            let m = cap.get(0).unwrap();
            let form_type = &cap[1];
            count_clauses(clean, form_type, m.start())
        })
        .sum()
}

pub fn cyclomatic_complexity(fn_text: &str) -> usize {
    let clean = without_strings_and_comments(fn_text);
    let simple = decision_re().find_iter(&clean).count();
    1 + simple + count_cond_decisions(&clean)
}

const NAME_STOP: &str = " \t\n\r()[]{}\"";

fn skip_space(form: &str, mut index: usize) -> usize {
    while let Some((ch, next)) = char_at(form, index) {
        if !ch.is_whitespace() {
            break;
        }
        index = next;
    }
    index
}

fn keyword_ends(form: &str, after: usize) -> bool {
    match char_at(form, after) {
        None => true,
        Some((ch, _)) => NAME_STOP.contains(ch),
    }
}

fn defn_keyword_end(form: &str, index: usize) -> Option<usize> {
    for candidate in ["defn-", "defn"] {
        if form.get(index..)?.starts_with(candidate) {
            let after = index + candidate.len();
            if keyword_ends(form, after) {
                return Some(after);
            }
        }
    }
    None
}

fn skip_metadata(form: &str, mut index: usize) -> usize {
    index += 1;
    if matches!(char_at(form, index), Some(('{', _))) {
        return balanced_end(form, index);
    }
    while let Some((ch, next)) = char_at(form, index) {
        if NAME_STOP.contains(ch) {
            break;
        }
        index = next;
    }
    index
}

fn skip_leading_metadata(form: &str, mut index: usize) -> usize {
    loop {
        index = skip_space(form, index);
        if matches!(char_at(form, index), Some(('^', _))) {
            index = skip_metadata(form, index);
            continue;
        }
        return index;
    }
}

fn read_name(form: &str, mut index: usize) -> Option<String> {
    let (first, _) = char_at(form, index)?;
    if "()[]{}\";".contains(first) {
        return None;
    }
    let start = index;
    while let Some((ch, next)) = char_at(form, index) {
        if NAME_STOP.contains(ch) {
            break;
        }
        index = next;
    }
    Some(form[start..index].to_string())
}

fn defn_name(form: &str) -> Option<String> {
    if !form.starts_with('(') {
        return None;
    }
    let keyword_at = defn_keyword_end(form, skip_space(form, 1))?;
    read_name(form, skip_leading_metadata(form, keyword_at))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClojureDefn {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

fn skip_reader(source: &str, index: usize, line: usize) -> Option<(usize, usize)> {
    let (ch, _) = char_at(source, index)?;
    if ch == ';' {
        return Some(consume_comment(source, index, line));
    }
    if ch == '"' {
        return Some(consume_string(source, index, line));
    }
    if ch == '\\' {
        return Some((char_literal_end(source, index), line));
    }
    if source[index..].starts_with("#_") {
        let end = form_end(source, index + 2);
        let newlines = source[index..end].bytes().filter(|&b| b == b'\n').count();
        return Some((end, line + newlines));
    }
    None
}

pub fn extract_top_level_defns(source: &str) -> Vec<RawClojureDefn> {
    let mut forms = Vec::new();
    let mut index = 0usize;
    let mut line = 1usize;
    let mut depth = 0usize;
    let mut form_start: Option<usize> = None;
    let mut form_line: Option<usize> = None;

    while index < source.len() {
        if let Some((next_idx, next_line)) = skip_reader(source, index, line) {
            index = next_idx;
            line = next_line;
            continue;
        }
        let Some((ch, next)) = char_at(source, index) else {
            break;
        };
        if ch == '(' {
            if depth == 0 {
                form_start = Some(index);
                form_line = Some(line);
            }
            depth += 1;
        } else if ch == ')' {
            if depth == 1 {
                if let (Some(start), Some(sline)) = (form_start, form_line) {
                    let form_text = &source[start..=index];
                    if let Some(name) = defn_name(form_text) {
                        forms.push(RawClojureDefn {
                            name,
                            start_line: sline,
                            end_line: line,
                            text: form_text.to_string(),
                        });
                    }
                }
                depth = 0;
                form_start = None;
                form_line = None;
            } else {
                depth = depth.saturating_sub(1);
            }
        }
        if ch == '\n' {
            line += 1;
        }
        index = next;
    }
    forms
}

pub fn extract_functions(source: &str) -> Vec<(String, usize, usize, usize)> {
    extract_top_level_defns(source)
        .into_iter()
        .map(|form| {
            let cc = cyclomatic_complexity(&form.text);
            (form.name, form.start_line, form.end_line, cc)
        })
        .collect()
}

pub fn declared_namespace(source: &str) -> Option<String> {
    let clean = without_strings_and_comments(source);
    for pat in [ns_re(), in_ns_re(), in_ns_quote_re()] {
        if let Some(cap) = pat.captures(&clean) {
            return Some(cap[1].to_string());
        }
    }
    None
}

pub fn namespace_from_path(path: &str, source_root: Option<&str>) -> String {
    let mut relative = path.replace('\\', "/");
    let root = source_root.unwrap_or("").replace('\\', "/");
    let root = root.trim_end_matches('/');
    if !root.is_empty()
        && root != "."
        && let Some(rest) = relative.strip_prefix(&format!("{root}/"))
    {
        relative = rest.to_string();
    }
    if let Some(rest) = relative.strip_prefix("src/") {
        relative = rest.to_string();
    } else if let Some((_, rest)) = relative.split_once("/src/") {
        relative = rest.to_string();
    }
    let stripped = clojure_ext_re().replace(&relative, "").into_owned();
    stripped.replace('/', ".").replace('_', "-")
}

pub fn functions_in_source(source: &str, path: &str, source_root: Option<&str>) -> Vec<Function> {
    let namespace =
        declared_namespace(source).unwrap_or_else(|| namespace_from_path(path, source_root));
    extract_functions(source)
        .into_iter()
        .map(|(name, start_line, end_line, complexity)| {
            Function::new(
                name,
                namespace.clone(),
                complexity,
                start_line,
                end_line,
                path,
                "clojure",
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_branch_and_cond_forms() {
        assert_eq!(cyclomatic_complexity("(defn foo [])"), 1);
        assert_eq!(cyclomatic_complexity("(defn foo [x]\n  (if x 1 0))"), 2);
        assert_eq!(
            cyclomatic_complexity("(defn foo [x]\n  (cond\n    (= x 1) :one\n    :else :other))"),
            3
        );
        assert_eq!(
            cyclomatic_complexity("(defn foo [x]\n  (some-> x inc dec))"),
            3
        );
        assert_eq!(
            cyclomatic_complexity("(defn foo [x]\n  (case x\n    1 :one\n    :other))"),
            3
        );
    }

    #[test]
    fn discards_metadata_and_char_literals() {
        let source = r#"
#_
; gone
(defn ghost [] (if x 1 0))
#_ "string"
#_ \x
#_ symbol
#_ #_(defn inner [] (if x 1 0))
(defn ^{:doc "hi" :private true} kept [] 1)
(defn ^{:flag \; :private true} also [] 1)
(def x #{\"})
(defn semi [] \; (if true 1 2))
"#;
        let fns = functions_in_source(source, "src/demo/core.clj", Some("/proj"));
        let summary: Vec<_> = fns
            .iter()
            .map(|f| (f.name.as_str(), f.complexity))
            .collect();
        assert_eq!(summary, [("kept", 1), ("also", 1), ("semi", 2)]);
        assert_eq!(fns[0].namespace, "demo.core");
    }

    #[test]
    fn handles_eof_cleanly() {
        assert!(extract_functions("(defn foo [] \\").is_empty());
        assert!(extract_functions("(defn foo [] \\abc").is_empty());
        assert!(extract_functions("(defn foo [] ;; comment").is_empty());
        assert!(extract_functions("(defn foo [] \"hello").is_empty());
    }
}
