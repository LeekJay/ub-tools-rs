use tree_sitter::Node;

use crate::languages::treesitter::{
    ancestor_names, child_of_type, complexity, descendants, end_line, node_text, parse, start_line,
    strip_root, strip_src,
};
use crate::model::Function;

const DECISIONS: &[&str] = &[
    "if_statement",
    "elif_clause",
    "for_statement",
    "while_statement",
    "except_clause",
    "conditional_expression",
    "case_clause",
    "if_clause",
];

fn is_decision(node: Node<'_>) -> bool {
    if DECISIONS.contains(&node.kind()) {
        return true;
    }
    if node.kind() != "boolean_operator" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| matches!(child.kind(), "and" | "or"))
}

fn skip(node: Node<'_>) -> bool {
    node.kind() == "class_definition"
}

fn strip_module_suffix(relative: &str) -> &str {
    let trimmed = relative.strip_suffix(".py").unwrap_or(relative);
    if let Some(prefix) = trimmed.strip_suffix("/__init__") {
        return prefix;
    }
    if trimmed == "__init__" {
        return "";
    }
    trimmed
}

fn module_namespace(path: &str, source_root: Option<&str>) -> String {
    let normalized = path.replace('\\', "/");
    let stripped = strip_src(&strip_root(&normalized, source_root));
    let module = strip_module_suffix(&stripped);
    if module.is_empty() {
        "__init__".to_string()
    } else {
        module.replace('/', ".")
    }
}

fn is_nested_function(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(cur) = current {
        if cur.kind() == "class_definition" {
            return false;
        }
        if cur.kind() == "function_definition" {
            return true;
        }
        current = cur.parent();
    }
    false
}

pub fn functions_in_source(source: &str, path: &str, source_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, "python") else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let module = module_namespace(path, source_root);
    let mut found = Vec::new();
    for node in descendants(tree.root_node()) {
        if node.kind() != "function_definition" || is_nested_function(node) {
            continue;
        }
        let Some(ident) = child_of_type(node, &["identifier"]) else {
            continue;
        };
        let classes = ancestor_names(data, node, "class_definition", &["identifier"]);
        let namespace = if classes.is_empty() {
            module.clone()
        } else if module.is_empty() {
            classes.join(".")
        } else {
            format!("{module}.{}", classes.join("."))
        };
        found.push(Function::new(
            node_text(data, ident),
            namespace,
            complexity(node, is_decision, skip),
            start_line(node),
            end_line(node),
            path,
            "python",
        ));
    }
    found
}
