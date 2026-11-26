//! Find mutation sites using tree-sitter across Clojure, Java, Go, TypeScript/JS, Rust, Python, and Swift.

use tree_sitter::{Language, Node, Parser};

/// Raw mutation site in UTF-8 byte offsets within a single source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSite {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub mutant: String,
    pub category: String,
    pub anchor_line: usize,
}

const BINARY_PARENTS: &[&str] = &[
    "binary_expression",
    "binary_operator",
    "comparison_operator",
    "boolean_operator",
    "additive_expression",
    "multiplicative_expression",
    "comparison_expression",
    "equality_expression",
    "infix_expression",
    "conjunction_expression",
    "disjunction_expression",
    "nil_coalescing_expression",
];

const UNARY_PARENTS: &[&str] = &[
    "unary_expression",
    "unary_operator",
    "not_operator",
    "prefix_expression",
];

const SWIFT_CONDITIONS: &[&str] = &[
    "if_statement",
    "guard_statement",
    "while_statement",
    "repeat_while_statement",
];

const SWIFT_SKIP: &[&str] = &[
    "if",
    "else",
    "guard",
    "while",
    "repeat",
    "{",
    "}",
    "statements",
    "if_statement",
    "comment",
];

const NUMBERS: &[&str] = &[
    "decimal_integer_literal",
    "integer_literal",
    "int_literal",
    "integer",
    "number",
    "num_lit",
];

const TYPE_NODES: &[&str] = &[
    "type_annotation",
    "type_alias_declaration",
    "interface_declaration",
    "type_arguments",
    "type_parameters",
    "literal_type",
];

/// Choose the tree-sitter grammar name for `(language, path)`.
pub fn grammar_for(language: &str, path: &str) -> String {
    if language == "typescript" {
        if path.ends_with(".tsx") {
            return "tsx".to_string();
        }
        if path.ends_with(".js")
            || path.ends_with(".jsx")
            || path.ends_with(".mjs")
            || path.ends_with(".cjs")
        {
            return "javascript".to_string();
        }
        return "typescript".to_string();
    }
    if language == "go" {
        return "go".to_string();
    }
    language.to_string()
}

fn ts_language(grammar: &str) -> Option<Language> {
    let lang: Language = match grammar {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "clojure" => tree_sitter_clojure_orchard::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

fn node_text<'a>(data: &'a [u8], node: Node<'_>) -> &'a str {
    std::str::from_utf8(&data[node.start_byte()..node.end_byte()]).unwrap_or("")
}

fn category(token: &str) -> &'static str {
    match token {
        "+" | "-" | "*" | "/" => "arithmetic",
        ">" | ">=" | "<" | "<=" => "comparison",
        "==" | "!=" | "===" | "!==" => "equality",
        "&&" | "||" | "??" | "and" | "or" => "logical",
        _ => "arithmetic",
    }
}

fn binary_mutant(language: &str, token: &str) -> Option<&'static str> {
    if language == "go" && token == "/" {
        return None;
    }
    if token == "??" && !matches!(language, "typescript" | "swift") {
        return None;
    }
    match token {
        "+" => Some("-"),
        "-" => Some("+"),
        "*" => Some("/"),
        "/" => Some("*"),
        ">" => Some(">="),
        ">=" => Some(">"),
        "<" => Some("<="),
        "<=" => Some("<"),
        "==" => Some("!="),
        "!=" => Some("=="),
        "===" => Some("!=="),
        "!==" => Some("==="),
        "&&" => Some("||"),
        "||" => Some("&&"),
        "??" => Some("||"),
        "and" => Some("or"),
        "or" => Some("and"),
        _ => None,
    }
}

fn boolean_mutant(token: &str) -> Option<&'static str> {
    match token {
        "true" => Some("false"),
        "false" => Some("true"),
        "True" => Some("False"),
        "False" => Some("True"),
        _ => None,
    }
}

fn clojure_head_mutant(token: &str) -> Option<(&'static str, &'static str)> {
    Some(match token {
        "+" => ("-", "arithmetic"),
        "-" => ("+", "arithmetic"),
        "*" => ("/", "arithmetic"),
        "inc" => ("dec", "arithmetic"),
        "dec" => ("inc", "arithmetic"),
        ">" => (">=", "comparison"),
        ">=" => (">", "comparison"),
        "<" => ("<=", "comparison"),
        "<=" => ("<", "comparison"),
        "=" => ("not=", "equality"),
        "not=" => ("=", "equality"),
        "if" => ("if-not", "conditional"),
        "if-not" => ("if", "conditional"),
        "when" => ("when-not", "conditional"),
        "when-not" => ("when", "conditional"),
        "and" => ("or", "logical"),
        "or" => ("and", "logical"),
        "double" => ("int", "coercion"),
        "int" => ("double", "coercion"),
        "first" => ("second", "seq"),
        "second" => ("first", "seq"),
        "filter" => ("remove", "seq"),
        "remove" => ("filter", "seq"),
        "take" => ("drop", "seq"),
        "drop" => ("take", "seq"),
        "rest" => ("next", "seq"),
        "next" => ("rest", "seq"),
        "every?" => ("some", "seq"),
        "some" => ("every?", "seq"),
        "min" => ("max", "numeric"),
        "max" => ("min", "numeric"),
        "pos?" => ("neg?", "predicate"),
        "neg?" => ("pos?", "predicate"),
        "even?" => ("odd?", "predicate"),
        "odd?" => ("even?", "predicate"),
        "nil?" => ("some?", "predicate"),
        "some?" => ("nil?", "predicate"),
        _ => return None,
    })
}

fn optional_mutant_kinds(parent_kind: &str, grandparent_kind: &str) -> Option<&'static str> {
    let host = if parent_kind == "optional_chain" {
        grandparent_kind
    } else {
        parent_kind
    };
    match host {
        "member_expression" => Some("."),
        "call_expression" | "subscript_expression" => Some(""),
        _ => None,
    }
}

fn is_statement(language: &str, node_type: &str) -> bool {
    // tree-sitter-python crates.io 0.23.6 wraps assignments/expressions in
    // `expression_statement`, unlike Python's tslp 1.25.2 supertype omission.
    if language == "python" && node_type == "expression_statement" {
        return false;
    }
    node_type.ends_with("_statement") || node_type.ends_with("_declaration")
}

fn add_site(
    found: &mut Vec<RawSite>,
    node: Node<'_>,
    original: &str,
    mutant: &str,
    category: &str,
    anchor_line: Option<usize>,
) {
    if mutant == original {
        return;
    }
    let line = node.start_position().row + 1;
    let resolved_anchor = match anchor_line {
        Some(a) if a > 0 => a,
        _ => line,
    };
    found.push(RawSite {
        line,
        start: node.start_byte(),
        end: node.end_byte(),
        original: original.to_string(),
        mutant: mutant.to_string(),
        category: category.to_string(),
        anchor_line: resolved_anchor,
    });
}

fn clojure_head<'a>(node: Node<'_>, data: &'a [u8]) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "sym_lit" {
            return Some(node_text(data, child));
        }
    }
    None
}

fn clojure_node_skipped(node: Node<'_>, data: &[u8]) -> bool {
    let kind = node.kind();
    if ["comment", "quot", "string", "str_lit", "char_lit"]
        .iter()
        .any(|part| kind.contains(part))
    {
        return true;
    }
    kind == "list_lit" && matches!(clojure_head(node, data), Some("comment" | "quote"))
}

fn flip01(text: &str) -> &'static str {
    if text == "0" { "1" } else { "0" }
}

fn visit_clojure(root: Node<'_>, data: &[u8], found: &mut Vec<RawSite>) {
    // Pass `is_head` and `skipped` down the stack so `node.parent()` is never called
    // (avoiding O(depth^2) tree-sitter parent walks on deeply nested forms).
    let mut stack = vec![(root, false, false)];
    while let Some((current, is_head, parent_skipped)) = stack.pop() {
        let skipped = parent_skipped || clojure_node_skipped(current, data);
        if skipped {
            continue;
        }
        let kind = current.kind();
        let text = match kind {
            "sym_lit" | "bool_lit" | "num_lit" => node_text(data, current),
            _ => "",
        };
        if kind == "sym_lit" && is_head {
            if let Some((mutant, cat)) = clojure_head_mutant(text) {
                add_site(found, current, text, mutant, cat, None);
            }
        } else if kind == "bool_lit" {
            if let Some(mutant) = boolean_mutant(text) {
                add_site(found, current, text, mutant, "boolean", None);
            }
        } else if kind == "num_lit" && matches!(text, "0" | "1") {
            add_site(found, current, text, flip01(text), "constant", None);
        }

        let is_list = kind == "list_lit";
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        let first_sym_id = if is_list {
            children
                .iter()
                .find(|c| c.kind() == "sym_lit")
                .map(Node::id)
        } else {
            None
        };
        for child in children.into_iter().rev() {
            let child_is_head = first_sym_id == Some(child.id());
            stack.push((child, child_is_head, false));
        }
    }
}

fn visit_leaf(
    node: Node<'_>,
    data: &[u8],
    language: &str,
    parent_kind: &str,
    grandparent_kind: &str,
    anchor_line: Option<usize>,
    found: &mut Vec<RawSite>,
) {
    let token = node_text(data, node);
    if BINARY_PARENTS.contains(&parent_kind)
        && let Some(mutant) = binary_mutant(language, token)
    {
        add_site(found, node, token, mutant, category(token), anchor_line);
    }
    if language == "typescript"
        && token == "?."
        && let Some(mutant) = optional_mutant_kinds(parent_kind, grandparent_kind)
    {
        add_site(found, node, "?.", mutant, "optional", anchor_line);
    }
    if matches!(token, "!" | "not")
        && (UNARY_PARENTS.contains(&parent_kind) || parent_kind == "comparison_operator")
    {
        add_site(found, node, token, "", "unary", anchor_line);
    }
    if token == "-" && UNARY_PARENTS.contains(&parent_kind) {
        add_site(found, node, token, "", "unary", anchor_line);
    }
    if matches!(node.kind(), "true" | "false")
        && let Some(mutant) = boolean_mutant(token)
    {
        add_site(found, node, token, mutant, "boolean", anchor_line);
    }
    if NUMBERS.contains(&node.kind()) && matches!(token, "0" | "1") {
        add_site(found, node, token, flip01(token), "constant", anchor_line);
    }
}

fn ends_swift_conditions(node_kind: &str, child_kind: &str) -> bool {
    if node_kind == "repeat_while_statement" {
        return false;
    }
    matches!(child_kind, "{" | "statements")
}

fn swift_conditions(node: Node<'_>) -> Vec<Node<'_>> {
    let kind = node.kind();
    if !SWIFT_CONDITIONS.contains(&kind) {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut binding = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let ck = child.kind();
        if ends_swift_conditions(kind, ck) {
            break;
        }
        if ck == "," {
            binding = false;
            continue;
        }
        if SWIFT_SKIP.contains(&ck) {
            continue;
        }
        if ck == "value_binding_pattern" {
            binding = true;
            continue;
        }
        if binding {
            continue;
        }
        found.push(child);
    }
    found
}

fn visit_swift_conditions(
    node: Node<'_>,
    data: &[u8],
    anchor_line: Option<usize>,
    found: &mut Vec<RawSite>,
) {
    for condition in swift_conditions(node) {
        let original = node_text(data, condition);
        if original.trim().is_empty() {
            continue;
        }
        let mutant = format!("!({original})");
        add_site(
            found,
            condition,
            original,
            &mutant,
            "conditional",
            anchor_line,
        );
    }
}

struct VisitFrame<'tree> {
    node: Node<'tree>,
    parent_kind: &'tree str,
    grandparent_kind: &'tree str,
    anchor_line: Option<usize>,
    in_ts_type: bool,
}

fn visit<'tree>(root: Node<'tree>, data: &[u8], language: &str, found: &mut Vec<RawSite>) {
    // Explicit stack carrying parent kinds, anchor statement line, and TypeScript
    // type-node state so we never call `node.parent()` (which walks from root in tree-sitter C).
    let mut stack = vec![VisitFrame {
        node: root,
        parent_kind: "",
        grandparent_kind: "",
        anchor_line: None,
        in_ts_type: false,
    }];
    while let Some(frame) = stack.pop() {
        let current = frame.node;
        if language == "typescript" && frame.in_ts_type {
            continue;
        }
        if language == "swift" {
            visit_swift_conditions(current, data, frame.anchor_line, found);
        }
        if current.child_count() == 0 {
            visit_leaf(
                current,
                data,
                language,
                frame.parent_kind,
                frame.grandparent_kind,
                frame.anchor_line,
                found,
            );
            continue;
        }
        if current.kind() == "bool_lit" {
            let text = node_text(data, current);
            if let Some(mutant) = boolean_mutant(text) {
                add_site(found, current, text, mutant, "boolean", frame.anchor_line);
            }
        }

        let current_kind = current.kind();
        let next_anchor = if is_statement(language, current_kind) {
            Some(current.start_position().row + 1)
        } else {
            frame.anchor_line
        };
        let next_in_type =
            frame.in_ts_type || (language == "typescript" && TYPE_NODES.contains(&current_kind));

        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(VisitFrame {
                node: child,
                parent_kind: current_kind,
                grandparent_kind: frame.parent_kind,
                anchor_line: next_anchor,
                in_ts_type: next_in_type,
            });
        }
    }
}

/// Discover mutation sites in source order with UTF-8 byte offsets.
pub fn discover_raw(source: &str, language: &str, path: &str) -> Result<Vec<RawSite>, String> {
    if language == "vue" {
        return Ok(Vec::new());
    }
    let grammar = grammar_for(language, path);
    let Some(lang) = ts_language(&grammar) else {
        return Err(format!("unsupported grammar: {grammar}"));
    };
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("set_language failed: {e}"))?;
    let data = source.as_bytes();
    let tree = parser
        .parse(data, None)
        .ok_or_else(|| "tree-sitter parse returned None".to_string())?;
    let mut found = Vec::new();
    if language == "clojure" {
        visit_clojure(tree.root_node(), data, &mut found);
    } else {
        visit(tree.root_node(), data, language, &mut found);
    }
    found.sort_by_key(|site| (site.line, site.start, site.end));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn pairs(sites: &[RawSite]) -> HashSet<(&str, &str)> {
        sites
            .iter()
            .map(|s| (s.original.as_str(), s.mutant.as_str()))
            .collect()
    }

    #[test]
    fn grammar_guards_and_all_languages() {
        assert_eq!(grammar_for("typescript", "src/app.tsx"), "tsx");
        assert_eq!(grammar_for("typescript", "src/app.ts"), "typescript");
        assert_eq!(grammar_for("typescript", "src/app.js"), "javascript");
        assert_eq!(grammar_for("typescript", "src/app.mjs"), "javascript");
        assert_eq!(grammar_for("typescript", "src/app.cjs"), "javascript");
        assert_eq!(grammar_for("typescript", "src/app.jsx"), "javascript");
        assert_eq!(grammar_for("python", "src/app.py"), "python");

        let clj = discover_raw(
            "(ns demo.core)\n(defn place [x]\n  (if (> x 0) (+ x 1) 0))\n(defn- hide [] false)\n",
            "clojure",
            "src/demo/core.clj",
        )
        .unwrap();
        let clj_pairs = pairs(&clj);
        assert!(clj_pairs.contains(&("if", "if-not")));
        assert!(clj_pairs.contains(&(">", ">=")));
        assert!(clj_pairs.contains(&("+", "-")));
        assert!(clj_pairs.contains(&("0", "1")));
        assert!(clj_pairs.contains(&("1", "0")));
        assert!(clj_pairs.contains(&("false", "true")));

        let clj_ignored = discover_raw(
            "(ns demo.core)\n(defn place []\n  (comment (+ 1 0))\n  (quote (+ 1 0))\n  '(+ 1 0)\n  (str \"true + 1\"))\n",
            "clojure",
            "src/demo/core.clj",
        )
        .unwrap();
        assert!(clj_ignored.is_empty());

        let ts = discover_raw(
            "type Flag = { on: true };\nexport function place(a, b) {\n  return a ?? b?.c ?? b?.[0] ?? b?.();\n}\n",
            "typescript",
            "src/demo/app.ts",
        )
        .unwrap();
        assert_eq!(ts.iter().filter(|s| s.original == "??").count(), 3);
        let opt: HashSet<&str> = ts
            .iter()
            .filter(|s| s.original == "?.")
            .map(|s| s.mutant.as_str())
            .collect();
        assert_eq!(opt, HashSet::from([".", ""]));
        assert!(!pairs(&ts).contains(&("true", "false")));

        let swift = discover_raw(
            "func place(x: Int, ready: Bool) -> Int {\n  guard x > 0, ready else { return 0 }\n  if let x, x > 1 { return 1 }\n  return x ?? 0\n}\n",
            "swift",
            "Sources/Demo/Box.swift",
        )
        .unwrap();
        let negated: HashSet<&str> = swift
            .iter()
            .filter(|s| s.mutant == format!("!({})", s.original))
            .map(|s| s.original.as_str())
            .collect();
        assert_eq!(negated, HashSet::from(["x > 0", "ready", "x > 1"]));

        let go = discover_raw(
            "package demo\nfunc Run(a, b int) int { return a * b + a / b }\n",
            "go",
            "demo.go",
        )
        .unwrap();
        let go_pairs = pairs(&go);
        assert!(go_pairs.contains(&("*", "/")));
        assert!(!go_pairs.contains(&("/", "*")));
    }

    #[test]
    fn deep_expression_and_python_multiline_assignment_anchor() {
        let expr = vec!["1"; 1200].join(" + ");
        let py_deep = format!("def place():\n    return {expr}\n");
        assert!(
            !discover_raw(&py_deep, "python", "app.py")
                .unwrap()
                .is_empty()
        );
        let go_deep = format!("package p\nfunc place() int {{ return {expr} }}\n");
        assert!(!discover_raw(&go_deep, "go", "app.go").unwrap().is_empty());

        let py_assign = "a = (\n    1 + 2\n)\n";
        let sites = discover_raw(py_assign, "python", "app.py").unwrap();
        let plus = sites.iter().find(|s| s.original == "+").unwrap();
        assert_eq!(plus.line, 2);
        assert_eq!(plus.anchor_line, 2);
    }
}
