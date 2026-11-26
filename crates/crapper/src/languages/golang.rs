use std::fs;
use std::path::Path;

use tree_sitter::Node;

use crate::languages::treesitter::{
    absolute_path, binary_logic, child_of_type, child_text, complexity, descendants, end_line,
    node_text, parse, start_line,
};
use crate::model::Function;
use crate::path_util::{resolve_path, to_posix};

const DECISIONS: &[&str] = &[
    "if_statement",
    "for_statement",
    "expression_case",
    "type_case",
    "communication_case",
    "default_case",
];

fn is_decision(node: Node<'_>) -> bool {
    DECISIONS.contains(&node.kind()) || binary_logic(node)
}

fn skip(_node: Node<'_>) -> bool {
    false
}

fn package_clause(data: &[u8], root: Node<'_>) -> String {
    for node in descendants(root) {
        if node.kind() == "package_identifier" {
            return node_text(data, node).to_string();
        }
    }
    String::new()
}

fn receiver_type(data: &[u8], method: Node<'_>) -> Option<String> {
    let params = child_of_type(method, &["parameter_list"])?;
    for item in descendants(params) {
        if item.kind() == "type_identifier" {
            return Some(node_text(data, item).to_string());
        }
    }
    None
}

pub fn package_namespace(path: &Path, package_name: &str) -> String {
    let resolved = resolve_path(path);
    let mut current = resolved.parent().map(Path::to_path_buf);
    let mut module_name: Option<String> = None;
    let mut module_dir = None;

    while let Some(dir) = current {
        let candidate = dir.join("go.mod");
        if candidate.is_file() {
            if let Ok(text) = fs::read_to_string(&candidate) {
                for line in text.lines() {
                    let stripped = line.trim();
                    if let Some(rest) = stripped.strip_prefix("module ") {
                        let name = rest.trim();
                        if !name.is_empty() {
                            module_name = Some(name.to_string());
                            module_dir = Some(dir.clone());
                        }
                        break;
                    }
                }
            }
            break;
        }
        current = dir.parent().filter(|&p| p != dir).map(Path::to_path_buf);
    }

    let (Some(module), Some(mod_dir)) = (module_name, module_dir) else {
        return package_name.to_string();
    };
    let parent = resolved.parent().unwrap_or(&resolved);
    let Ok(rel) = parent.strip_prefix(&mod_dir) else {
        return package_name.to_string();
    };
    let rel_posix = to_posix(rel);
    if rel_posix.is_empty() || rel_posix == "." {
        module
    } else {
        format!("{module}/{rel_posix}")
    }
}

pub fn functions_in_source(source: &str, path: &str, project_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, "go") else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let package_name = package_clause(data, tree.root_node());
    let abs = absolute_path(path, project_root);
    let package_ns = package_namespace(&abs, &package_name);
    let mut found = Vec::new();

    for node in descendants(tree.root_node()) {
        if !matches!(node.kind(), "function_declaration" | "method_declaration") {
            continue;
        }
        if child_of_type(node, &["block"]).is_none() {
            continue;
        }
        let (name, namespace) = if node.kind() == "method_declaration" {
            let Some(mname) = child_text(data, node, &["field_identifier"]) else {
                continue;
            };
            let Some(receiver) = receiver_type(data, node) else {
                continue;
            };
            (mname.to_string(), format!("{package_ns}.{receiver}"))
        } else {
            let Some(fname) = child_text(data, node, &["identifier"]) else {
                continue;
            };
            (fname.to_string(), package_ns.clone())
        };
        found.push(Function::new(
            name,
            namespace,
            complexity(node, is_decision, skip),
            start_line(node),
            end_line(node),
            path,
            "go",
        ));
    }
    found
}
