//! Pick candidate forms for each supported language and normalize them into entries.

use tree_sitter::Node;

use super::astnorm::normalize_with_options;
use super::clojure_read::{is_candidate_form, max_line, normalize as normalize_clj, read_source};
use super::model::Entry;
use super::shape::{fingerprints, node_count};
use super::treesitter::{child_of_type, descendants, end_line, node_text, parse, start_line};
use super::vue::visit_vue_nodes;

const VUE_COMPARISON_LANGUAGE: &str = "typescript";

fn ts_grammar(path: &str) -> &'static str {
    if path.ends_with(".tsx") {
        "tsx"
    } else {
        "typescript"
    }
}

fn has_child(node: Node<'_>, types: &[&str]) -> bool {
    child_of_type(node, types).is_some()
}

fn inside(node: Node<'_>, types: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if types.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn is_ts_function(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
    )
}

const TS_FUNCTIONS: &[&str] = &[
    "function_declaration",
    "method_definition",
    "arrow_function",
    "function_expression",
];

const DEFINITION_PARENTS: &[&str] = &[
    "variable_declarator",
    "public_field_definition",
    "property_definition",
];

fn python_nested(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition" => return false,
            "function_definition" => return true,
            _ => current = parent.parent(),
        }
    }
    false
}

fn rust_in_tests(data: &[u8], node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item"
            && let Some(ident) = child_of_type(parent, &["identifier"])
            && node_text(data, ident) == "tests"
        {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn java_nodes<'a>(_data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_declaration" {
            let nested = inside(
                node,
                &[
                    "method_declaration",
                    "constructor_declaration",
                    "lambda_expression",
                ],
            );
            if has_child(node, &["block"]) && !nested {
                found.push(node);
            }
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'a>> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    found
}

fn go_nodes<'a>(_data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    descendants(root)
        .into_iter()
        .filter(|node| {
            matches!(node.kind(), "function_declaration" | "method_declaration")
                && has_child(*node, &["block"])
        })
        .collect()
}

fn python_nodes<'a>(_data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    descendants(root)
        .into_iter()
        .filter(|node| node.kind() == "function_definition" && !python_nested(*node))
        .collect()
}

fn keep_typescript_function(node: Node<'_>) -> bool {
    has_child(node, &["statement_block"]) && !inside(node, TS_FUNCTIONS)
}

fn typescript_parent(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    if !DEFINITION_PARENTS.contains(&parent.kind()) {
        return None;
    }
    if inside(parent, TS_FUNCTIONS) {
        return None;
    }
    Some(parent)
}

pub fn typescript_nodes<'a>(_data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for node in descendants(root) {
        if matches!(node.kind(), "function_declaration" | "method_definition") {
            if keep_typescript_function(node) {
                found.push(node);
            }
        } else if matches!(node.kind(), "arrow_function" | "function_expression")
            && let Some(parent) = typescript_parent(node)
        {
            found.push(parent);
        }
    }
    let _ = is_ts_function;
    found
}

const SWIFT_NEST: &[&str] = &[
    "function_declaration",
    "protocol_function_declaration",
    "lambda_literal",
    "computed_property",
    "init_declaration",
    "subscript_declaration",
    "deinit_declaration",
];

fn swift_nodes<'a>(_data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for node in descendants(root) {
        if inside(node, SWIFT_NEST) {
            continue;
        }
        let is_candidate = matches!(node.kind(), "function_declaration" | "deinit_declaration")
            || (node.kind() == "protocol_function_declaration" && has_child(node, &["statements"]))
            || (node.kind() == "property_declaration"
                && has_child(node, &["computed_property", "lambda_literal"]));
        if is_candidate {
            found.push(node);
        }
    }
    found
}

fn rust_nodes<'a>(data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for node in descendants(root) {
        if node.kind() != "function_item" || !has_child(node, &["block"]) {
            continue;
        }
        if node.parent().is_some_and(|p| p.kind() == "block") {
            continue;
        }
        if rust_in_tests(data, node) {
            continue;
        }
        found.push(node);
    }
    found
}

fn entry_from_node(language: &str, file: &str, data: &[u8], node: Node<'_>) -> Option<Entry> {
    let unwrap_python_expr_stmt = language == "python";
    let normalized = normalize_with_options(&node, data, unwrap_python_expr_stmt)?;
    Some(Entry {
        language: language.to_string(),
        file: file.to_string(),
        start_line: start_line(node),
        end_line: end_line(node),
        nodes: node_count(&normalized),
        fingerprints: fingerprints(&normalized),
    })
}

fn tree_entries(language: &str, source: &str, path: &str, file: &str) -> Vec<Entry> {
    let grammar = match language {
        "typescript" => ts_grammar(path),
        "java" => "java",
        "go" => "go",
        "python" => "python",
        "rust" => "rust",
        "swift" => "swift",
        _ => return Vec::new(),
    };
    let Ok((data, tree)) = parse(source, grammar) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let picked = match language {
        "java" => java_nodes(&data, root),
        "go" => go_nodes(&data, root),
        "python" => python_nodes(&data, root),
        "typescript" => typescript_nodes(&data, root),
        "rust" => rust_nodes(&data, root),
        "swift" => swift_nodes(&data, root),
        _ => Vec::new(),
    };
    picked
        .into_iter()
        .filter_map(|node| entry_from_node(language, file, &data, node))
        .collect()
}

pub fn clojure_entries(source: &str, file: &str) -> (Vec<Entry>, Option<String>) {
    let (forms, warning) = read_source(source);
    let mut entries = Vec::new();
    for form in &forms {
        if !is_candidate_form(form) {
            continue;
        }
        let normalized = normalize_clj(form);
        entries.push(Entry {
            language: "clojure".to_string(),
            file: file.to_string(),
            start_line: form.line(),
            end_line: max_line(form),
            nodes: node_count(&normalized),
            fingerprints: fingerprints(&normalized),
        });
    }
    (entries, warning)
}

pub fn entries_in_source(
    language: &str,
    source: &str,
    path: &str,
    file: &str,
) -> (Vec<Entry>, Option<String>) {
    if language == "clojure" {
        return clojure_entries(source, file);
    }
    if language == "vue" {
        let mut entries = Vec::new();
        visit_vue_nodes(source, typescript_nodes, |data, node| {
            if let Some(entry) = entry_from_node(VUE_COMPARISON_LANGUAGE, file, data, node) {
                entries.push(entry);
            }
        });
        return (entries, None);
    }
    (tree_entries(language, source, path, file), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::find_duplicates;
    use crate::vue::extract_script_blocks;

    #[test]
    fn python_node_count_and_deep_expression_match_python_tslp() {
        let left = "def alpha(xs):\n    ys = filter(xs, odd)\n    zs = map(ys, inc)\n    return list(zs)\n";
        let right = "def beta(items):\n    kept = filter(items, even)\n    out = map(kept, dec)\n    return list(out)\n";
        let (mut entries, w1) = entries_in_source("python", left, "src/left.py", "src/left.py");
        let (r_entries, w2) = entries_in_source("python", right, "src/right.py", "src/right.py");
        assert!(w1.is_none() && w2.is_none());
        assert_eq!(entries[0].nodes, 48);
        assert_eq!(r_entries[0].nodes, 48);
        entries.extend(r_entries);
        let dups = find_duplicates(&entries, 0.99);
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].score, 1.0);

        let expr = vec!["1"; 1200].join(" + ");
        let deep = format!("def place():\n    return {expr}\n");
        let (deep_entries, _) = entries_in_source("python", &deep, "app.py", "app.py");
        assert_eq!(deep_entries.len(), 1);
    }

    #[test]
    fn all_tree_sitter_languages_match_renamed_locals_and_exclusions() {
        let pairs = [
            (
                "java",
                "a.java",
                "class Left { int alpha(int[] xs) { int[] ys = filter(xs, odd); return map(ys, inc); } }",
                "b.java",
                "class Right { int beta(int[] items) { int[] kept = filter(items, even); return map(kept, dec); } }",
            ),
            (
                "go",
                "a.go",
                "package demo\nfunc (b *Board) alpha(xs []int) []int {\n\tys := filter(xs, odd)\n\treturn mapFn(ys, inc)\n}\n",
                "b.go",
                "package demo\nfunc (p *Panel) beta(items []int) []int {\n\tkept := filter(items, even)\n\treturn mapFn(kept, dec)\n}\n",
            ),
            (
                "typescript",
                "a.tsx",
                "export const alpha = (xs: number[]): number[] => {\n  const ys = filter(xs, odd);\n  return map(ys, inc);\n};\n",
                "b.tsx",
                "export const beta = (items: number[]): number[] => {\n  const kept = filter(items, even);\n  return map(kept, dec);\n};\n",
            ),
            (
                "rust",
                "a.rs",
                "pub fn alpha(xs: Vec<i32>) -> Vec<i32> {\n    let ys = filter(xs, odd);\n    map(ys, inc)\n}\nmod tests {\n    fn ignored() { let x = 1; }\n}\n",
                "b.rs",
                "pub fn beta(items: Vec<i32>) -> Vec<i32> {\n    let kept = filter(items, even);\n    map(kept, dec)\n}\n",
            ),
            (
                "swift",
                "a.swift",
                "func alpha(xs: [Int]) -> [Int] {\n    let ys = filter(xs, odd)\n    return map(ys, inc)\n}\n",
                "b.swift",
                "func beta(items: [Int]) -> [Int] {\n    let kept = filter(items, even)\n    return map(kept, dec)\n}\n",
            ),
        ];

        for (lang, fa, sa, fb, sb) in pairs {
            let (mut ea, _) = entries_in_source(lang, sa, fa, fa);
            let (eb, _) = entries_in_source(lang, sb, fb, fb);
            assert_eq!(ea.len(), 1, "{lang} left count");
            assert_eq!(eb.len(), 1, "{lang} right count");
            ea.extend(eb);
            let dups = find_duplicates(&ea, 0.99);
            assert_eq!(dups.len(), 1, "{lang} dup");
            assert_eq!(dups[0].score, 1.0, "{lang} score");
        }
    }

    #[test]
    fn vue_extracts_setup_options_bom_and_compares_with_ts() {
        let vue_src = "\u{feff}<template><p>你好 🚀</p></template>\r\n<script setup lang=\"ts\">\r\nconst greet = (xs: number[]) => {\r\n  const ys = filter(xs, odd)\r\n  return map(ys, inc)\r\n}\r\n</script>\r\n<script>export default { data: () => ({ label: '中文' }) }</script>\r\n";
        let (_data, blocks) = extract_script_blocks(vue_src).unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].is_setup);
        assert!(!blocks[1].is_setup);

        let (mut vue_e, _) = entries_in_source("vue", vue_src, "Card.vue", "Card.vue");
        assert_eq!(
            vue_e
                .iter()
                .map(|e| (e.start_line, e.end_line))
                .collect::<Vec<_>>(),
            vec![(3, 6), (8, 8)]
        );
        let ts_src = "export const hello = (items: number[]) => {\n  const kept = filter(items, even);\n  return map(kept, dec);\n};\n";
        let (ts_e, _) = entries_in_source("typescript", ts_src, "h.ts", "h.ts");
        vue_e.extend(ts_e);
        let dups = find_duplicates(&vue_e, 0.99);
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].language, "typescript");
    }

    #[test]
    fn swift_labels_operators_ranges_and_deinit_match_rules() {
        let src_a = "func alpha(xs: [Int]) -> Bool {\n    let ys = move(from: xs, to: xs)\n    return xs >= 1 && xs..<2\n}\n";
        let src_b = "func beta(items: [Int]) -> Bool {\n    let kept = move(from: items, to: items)\n    return items >= 9 && items..<8\n}\n";
        let src_c = "func gamma(items: [Int]) -> Bool {\n    let kept = move(start: items, end: items)\n    return items != 9 && items...8\n}\n";
        let (mut ea, _) = entries_in_source("swift", src_a, "a.swift", "a.swift");
        let (eb, _) = entries_in_source("swift", src_b, "b.swift", "b.swift");
        let (ec, _) = entries_in_source("swift", src_c, "c.swift", "c.swift");
        let mut ab = ea.clone();
        ab.extend(eb);
        assert_eq!(find_duplicates(&ab, 0.99).len(), 1);
        ea.extend(ec);
        let ac = find_duplicates(&ea, 0.0);
        assert!(ac[0].score < 1.0);
    }
}
