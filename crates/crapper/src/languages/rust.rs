use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use tree_sitter::Node;

use crate::languages::treesitter::{
    absolute_path, ancestor_names, binary_logic, child_of_type, complexity, descendants, end_line,
    node_text, parse, start_line,
};
use crate::model::Function;
use crate::path_util::{resolve_path, to_posix};

const DECISIONS: &[&str] = &[
    "if_expression",
    "for_expression",
    "while_expression",
    "loop_expression",
    "match_arm",
    "try_expression",
];

fn package_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?ms)^\[package\](.*?)(?:^\[|\z)").expect("valid Cargo.toml package regex")
    })
}

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^name\s*=\s*"([^"]+)""#).expect("valid package name regex"))
}

fn is_decision(node: Node<'_>) -> bool {
    DECISIONS.contains(&node.kind()) || binary_logic(node)
}

fn skip(_node: Node<'_>) -> bool {
    false
}

fn has_block(node: Node<'_>) -> bool {
    child_of_type(node, &["block"]).is_some()
}

fn ancestor_mods(data: &[u8], node: Node<'_>) -> Vec<String> {
    ancestor_names(data, node, "mod_item", &["identifier"])
}

fn in_mod_named(data: &[u8], node: Node<'_>, name: &str) -> bool {
    ancestor_mods(data, node).iter().any(|m| m == name)
}

fn first_type_name(data: &[u8], node: Node<'_>) -> Option<String> {
    if node.kind() == "type_identifier" {
        return Some(node_text(data, node).to_string());
    }
    for item in descendants(node) {
        if item.kind() == "type_identifier" {
            return Some(node_text(data, item).to_string());
        }
    }
    None
}

fn impl_type(data: &[u8], impl_node: Node<'_>) -> Option<String> {
    let mut saw_for = false;
    let mut chosen = None;
    let mut cursor = impl_node.walk();
    for child in impl_node.children(&mut cursor) {
        if child.kind() == "for" {
            saw_for = true;
            chosen = None;
            continue;
        }
        if matches!(
            child.kind(),
            "type_identifier" | "generic_type" | "scoped_type_identifier"
        ) {
            chosen = first_type_name(data, child);
            if saw_for {
                return chosen;
            }
        }
    }
    chosen
}

fn crate_name(path: &Path) -> (String, Option<PathBuf>) {
    let resolved = resolve_path(path);
    let mut current = resolved.parent().map(Path::to_path_buf);
    while let Some(dir) = current {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file()
            && let Ok(text) = fs::read_to_string(&cargo)
            && let Some(block) = package_block_re().captures(&text)
        {
            let name = name_re()
                .captures(&block[1])
                .map(|c| c[1].to_string())
                .unwrap_or_else(|| {
                    dir.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("crate")
                        .to_string()
                });
            return (name.replace('-', "_"), Some(dir));
        }
        current = dir.parent().filter(|&p| p != dir).map(Path::to_path_buf);
    }
    ("crate".to_string(), None)
}

fn rs_parts(relative: &str) -> Vec<String> {
    let mut parts: Vec<String> = relative.split('/').map(str::to_string).collect();
    if parts.last().map(String::as_str) == Some("mod.rs") {
        parts.pop();
        return parts;
    }
    if let Some(last) = parts.last_mut()
        && let Some(stripped) = last.strip_suffix(".rs")
    {
        *last = stripped.to_string();
    }
    parts
}

fn with_crate(crate_name: &str, parts: Vec<String>) -> Vec<String> {
    if parts.first().map(String::as_str) == Some("bin") && parts.len() >= 2 {
        return parts.into_iter().skip(1).collect();
    }
    let mut out = vec![crate_name.to_string()];
    out.extend(parts);
    out
}

pub fn file_modules(path: &Path, crate_name: &str, crate_root: Option<&Path>) -> Vec<String> {
    let file_path = resolve_path(path);
    let Some(root) = crate_root else {
        let stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(stem, "lib" | "main") {
            return vec![crate_name.to_string()];
        }
        return vec![crate_name.to_string(), stem.to_string()];
    };
    let resolved_root = resolve_path(root);
    let Ok(rel_path) = file_path.strip_prefix(&resolved_root) else {
        return vec![crate_name.to_string()];
    };
    let mut relative = to_posix(rel_path);
    if let Some(rest) = relative.strip_prefix("src/") {
        relative = rest.to_string();
    }
    if matches!(relative.as_str(), "lib.rs" | "main.rs") {
        return vec![crate_name.to_string()];
    }
    with_crate(crate_name, rs_parts(&relative))
}

fn namespace(modules: &[String], extra: &[String], type_name: Option<&str>) -> String {
    let mut parts: Vec<&str> = modules
        .iter()
        .chain(extra.iter())
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(t) = type_name.filter(|s| !s.is_empty()) {
        parts.push(t);
    }
    parts.join("::")
}

fn impl_owner(data: &[u8], node: Node<'_>) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "declaration_list" {
        return None;
    }
    let owner = parent.parent()?;
    if owner.kind() != "impl_item" {
        return None;
    }
    impl_type(data, owner)
}

fn record_function(
    data: &[u8],
    node: Node<'_>,
    modules: &[String],
    path: &str,
) -> Option<Function> {
    if node.kind() != "function_item" || !has_block(node) {
        return None;
    }
    if node.parent().is_some_and(|p| p.kind() == "block") {
        return None;
    }
    if in_mod_named(data, node, "tests") {
        return None;
    }
    let ident = child_of_type(node, &["identifier"])?;
    let extra = ancestor_mods(data, node);
    let owner = impl_owner(data, node);
    Some(Function::new(
        node_text(data, ident),
        namespace(modules, &extra, owner.as_deref()),
        complexity(node, is_decision, skip),
        start_line(node),
        end_line(node),
        path,
        "rust",
    ))
}

pub fn functions_in_source(source: &str, path: &str, project_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, "rust") else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let located = absolute_path(path, project_root);
    let (cname, croot) = crate_name(&located);
    let modules = file_modules(&located, &cname, croot.as_deref());
    descendants(tree.root_node())
        .into_iter()
        .filter_map(|node| record_function(data, node, &modules, path))
        .collect()
}
