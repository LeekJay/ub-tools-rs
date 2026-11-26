use std::sync::OnceLock;

use regex::Regex;
use tree_sitter::Node;

use crate::languages::treesitter::{
    binary_logic, child_of_type, complexity, end_line, node_text, parse, start_line,
};
use crate::model::Function;

fn package_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*package\s+([a-zA-Z_][\w.]*)\s*;").expect("valid java pkg regex")
    })
}

const NESTED_TYPES: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
];

const DECISIONS: &[&str] = &[
    "if_statement",
    "for_statement",
    "enhanced_for_statement",
    "while_statement",
    "do_statement",
    "catch_clause",
    "ternary_expression",
    "switch_label",
];

fn is_decision(node: Node<'_>) -> bool {
    DECISIONS.contains(&node.kind()) || binary_logic(node)
}

fn skip(node: Node<'_>) -> bool {
    NESTED_TYPES.contains(&node.kind()) || node.kind() == "class_body"
}

fn inside_executable(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(cur) = current {
        if matches!(
            cur.kind(),
            "method_declaration" | "constructor_declaration" | "lambda_expression"
        ) {
            return true;
        }
        current = cur.parent();
    }
    false
}

fn package_name(source: &str) -> Option<&str> {
    package_re()
        .captures(source)
        .and_then(|c| c.get(1).map(|m| m.as_str()))
}

fn type_chain(data: &[u8], node: Node<'_>) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(cur) = current {
        if NESTED_TYPES.contains(&cur.kind())
            && let Some(ident) = child_of_type(cur, &["identifier"])
        {
            names.push(node_text(data, ident).to_string());
        }
        current = cur.parent();
    }
    names.reverse();
    names
}

fn qualify(package: Option<&str>, names: &[String], nested: &str) -> String {
    let body = names.join(nested);
    match package {
        Some(pkg) if !body.is_empty() => format!("{pkg}.{body}"),
        Some(pkg) => pkg.to_string(),
        None => body,
    }
}

pub fn functions_in_source(source: &str, path: &str, _project_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, "java") else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let package = package_name(source);
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        if node.kind() == "method_declaration" && !inside_executable(node) {
            if child_of_type(node, &["block"]).is_some() {
                let ident = child_of_type(node, &["identifier"]);
                let names = type_chain(data, node);
                if let Some(id_node) = ident
                    && !names.is_empty()
                {
                    let mut func = Function::new(
                        node_text(data, id_node),
                        qualify(package, &names, "."),
                        complexity(node, is_decision, skip),
                        start_line(node),
                        end_line(node),
                        path,
                        "java",
                    );
                    func.jacoco_class = Some(qualify(package, &names, "$"));
                    found.push(func);
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    found
}
