use std::fs;
use std::path::Path;

use tree_sitter::Node;

use crate::languages::treesitter::{
    absolute_path, child_of_type, child_text, complexity, descendants, end_line, node_text, parse,
    start_line,
};
use crate::model::Function;
use crate::path_util::resolve_path;
use crate::runners::nearest_marker;

const DECISIONS: &[&str] = &[
    "if_statement",
    "guard_statement",
    "for_statement",
    "while_statement",
    "repeat_while_statement",
    "catch_block",
    "ternary_expression",
    "switch_entry",
    "conjunction_expression",
    "disjunction_expression",
    "nil_coalescing_expression",
];

const NOT_CHAIN: &[&str] = &[
    "optional_type",
    "ternary_expression",
    "try_operator",
    "init_declaration",
    "?",
];

const TYPES: &[&str] = &["class_declaration", "protocol_declaration"];

const FUNCTIONS: &[&str] = &[
    "function_declaration",
    "protocol_function_declaration",
    "init_declaration",
    "deinit_declaration",
];

const CONTAINERS: &[&str] = &[
    "function_declaration",
    "protocol_function_declaration",
    "init_declaration",
    "deinit_declaration",
    "subscript_declaration",
    "lambda_literal",
    "computed_property",
    "computed_getter",
    "computed_setter",
    "computed_modify",
    "willset_clause",
    "didset_clause",
];

const TARGET_NAMES: &[&str] = &["target", "executableTarget"];

fn is_decision(node: Node<'_>) -> bool {
    if DECISIONS.contains(&node.kind()) {
        return true;
    }
    if node.kind() != "?" {
        return false;
    }
    node.parent()
        .is_some_and(|p| !NOT_CHAIN.contains(&p.kind()))
}

fn complexity_of(node: Node<'_>) -> usize {
    let root_id = node.id();
    complexity(node, is_decision, |current| {
        current.id() != root_id && TYPES.contains(&current.kind())
    })
}

fn is_nested(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(cur) = current {
        if TYPES.contains(&cur.kind()) {
            return false;
        }
        if CONTAINERS.contains(&cur.kind()) {
            return true;
        }
        current = cur.parent();
    }
    false
}

fn fixed_name(kind: &str) -> Option<&'static str> {
    match kind {
        "init_declaration" => Some("init"),
        "deinit_declaration" => Some("deinit"),
        "subscript_declaration" => Some("subscript"),
        _ => None,
    }
}

fn function_name(data: &[u8], node: Node<'_>) -> Option<String> {
    if let Some(fixed) = fixed_name(node.kind()) {
        return Some(fixed.to_string());
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    for (idx, child) in children.iter().enumerate() {
        if child.kind() == "func"
            && let Some(next) = children.get(idx + 1)
        {
            return Some(node_text(data, *next).to_string());
        }
    }
    None
}

fn named_child(data: &[u8], node: Node<'_>, kind: &str) -> Option<String> {
    let child = child_of_type(node, &[kind])?;
    child_text(data, child, &["simple_identifier"]).map(str::to_string)
}

fn has_function_body(node: Node<'_>) -> bool {
    child_of_type(node, &["function_body"]).is_some()
}

fn direct_lambda(node: Node<'_>) -> Option<Node<'_>> {
    child_of_type(node, &["lambda_literal"])
}

fn has_statements(node: Node<'_>) -> bool {
    if node.kind() == "statements" {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(has_statements)
}

fn property_is_entry(node: Node<'_>) -> bool {
    if direct_lambda(node).is_some() {
        return true;
    }
    let Some(accessor) = child_of_type(node, &["computed_property", "willset_didset_block"]) else {
        return false;
    };
    has_statements(accessor)
}

fn type_identifiers(data: &[u8], node: Node<'_>) -> Vec<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.kind() == "type_identifier")
        .map(|c| node_text(data, c).to_string())
        .collect()
}

fn type_name(data: &[u8], node: Node<'_>) -> Option<String> {
    if let Some(user) = child_of_type(node, &["user_type"]) {
        let parts = type_identifiers(data, user);
        if !parts.is_empty() {
            return Some(parts.join("."));
        }
    }
    child_text(data, node, &["type_identifier"]).map(str::to_string)
}

fn owner_types(data: &[u8], node: Node<'_>) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(cur) = current {
        if TYPES.contains(&cur.kind())
            && let Some(name) = type_name(data, cur)
        {
            names.push(name);
        }
        current = cur.parent();
    }
    names.reverse();
    names
}

fn namespace(module: &str, data: &[u8], node: Node<'_>) -> String {
    let owners = owner_types(data, node);
    if owners.is_empty() {
        module.to_string()
    } else if module.is_empty() {
        owners.join(".")
    } else {
        format!("{module}.{}", owners.join("."))
    }
}

fn make_function(node: Node<'_>, name: String, namespace: String, path: &str) -> Function {
    let mut func = Function::new(
        name,
        namespace,
        complexity_of(node),
        start_line(node),
        end_line(node),
        path,
        "swift",
    );
    func.start_byte = Some(node.start_byte());
    func.end_byte = Some(node.end_byte());
    func
}

fn record_function(data: &[u8], node: Node<'_>, module: &str, path: &str) -> Option<Function> {
    if !has_function_body(node) || is_nested(node) {
        return None;
    }
    let name = function_name(data, node)?;
    Some(make_function(
        node,
        name,
        namespace(module, data, node),
        path,
    ))
}

fn record_subscript(data: &[u8], node: Node<'_>, module: &str, path: &str) -> Option<Function> {
    if is_nested(node) || !has_statements(node) {
        return None;
    }
    Some(make_function(
        node,
        "subscript".to_string(),
        namespace(module, data, node),
        path,
    ))
}

fn record_property(data: &[u8], node: Node<'_>, module: &str, path: &str) -> Option<Function> {
    if is_nested(node) || !property_is_entry(node) {
        return None;
    }
    let name = named_child(data, node, "pattern")?;
    Some(make_function(
        node,
        name,
        namespace(module, data, node),
        path,
    ))
}

fn recorded(data: &[u8], node: Node<'_>, module: &str, path: &str) -> Option<Function> {
    if FUNCTIONS.contains(&node.kind()) {
        record_function(data, node, module, path)
    } else if node.kind() == "subscript_declaration" {
        record_subscript(data, node, module, path)
    } else if node.kind() == "property_declaration" {
        record_property(data, node, module, path)
    } else {
        None
    }
}

fn parts(path: &str) -> Vec<String> {
    path.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .map(str::to_string)
        .collect()
}

fn sources_name(parts: &[String]) -> Option<String> {
    let index = parts.iter().position(|p| p == "Sources")?;
    if index + 1 >= parts.len().saturating_sub(1) {
        return None;
    }
    parts.get(index + 1).cloned()
}

fn directory_module(path: &str, project_root: Option<&str>) -> String {
    let p = parts(path);
    if let Some(sources) = sources_name(&p) {
        return sources;
    }
    let mut dirs = if p.len() > 1 {
        &p[..p.len() - 1]
    } else {
        &[][..]
    };
    if dirs.first().map(String::as_str) == Some("src") {
        dirs = &dirs[1..];
    }
    if !dirs.is_empty() {
        return dirs.join(".");
    }
    if let Some(root) = project_root.filter(|s| !s.is_empty())
        && let Some(name) = Path::new(root).file_name().and_then(|n| n.to_str())
    {
        return name.to_string();
    }
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn string_parts(data: &[u8], node: Node<'_>, quote: &str, text_type: &str) -> Option<String> {
    let mut out = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == quote {
            continue;
        }
        if child.kind() != text_type {
            return None;
        }
        out.push_str(node_text(data, child));
    }
    Some(out)
}

fn plain_string(data: &[u8], node: Node<'_>) -> Option<String> {
    match node.kind() {
        "line_string_literal" => string_parts(data, node, "\"", "line_str_text"),
        "multi_line_string_literal" => {
            string_parts(data, node, "\"\"\"", "multi_line_str_text").map(|s| s.trim().to_string())
        }
        _ => None,
    }
}

fn suffix_name<'a>(data: &'a [u8], node: Node<'a>) -> Option<&'a str> {
    let mut suffix = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "navigation_suffix" {
            suffix = Some(child);
        }
    }
    child_text(data, suffix?, &["simple_identifier"])
}

fn call_name<'a>(data: &'a [u8], node: Node<'a>) -> Option<&'a str> {
    let callee = child_of_type(node, &["prefix_expression", "navigation_expression"])?;
    if callee.kind() == "prefix_expression" {
        child_text(data, callee, &["simple_identifier"])
    } else {
        suffix_name(data, callee)
    }
}

fn labeled_string(data: &[u8], call: Node<'_>, label: &str) -> Option<String> {
    let suffix = child_of_type(call, &["call_suffix"])?;
    let arguments = child_of_type(suffix, &["value_arguments"])?;
    let mut arg_cursor = arguments.walk();
    for argument in arguments.children(&mut arg_cursor) {
        if argument.kind() != "value_argument"
            || named_child(data, argument, "value_argument_label").as_deref() != Some(label)
        {
            continue;
        }
        let mut val_cursor = argument.walk();
        for child in argument.children(&mut val_cursor) {
            if let Some(text) = plain_string(data, child) {
                return Some(text);
            }
        }
    }
    None
}

fn target_calls<'a>(data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut calls = Vec::new();
    for node in descendants(root) {
        if node.kind() == "call_expression"
            && let Some(name) = call_name(data, node)
            && TARGET_NAMES.contains(&name)
        {
            calls.push(node);
        }
    }
    calls.sort_by_key(|n| (n.end_byte(), n.start_byte()));
    calls
}

fn package_targets(text: &str) -> Vec<(String, Option<String>)> {
    let Ok(tree) = parse(text, "swift") else {
        return Vec::new();
    };
    let data = text.as_bytes();
    let mut found = Vec::new();
    for node in target_calls(data, tree.root_node()) {
        if let Some(name) = labeled_string(data, node, "name")
            && !name.is_empty()
        {
            found.push((name, labeled_string(data, node, "path")));
        }
    }
    found
}

fn best_target(
    package_dir: &Path,
    file: &Path,
    targets: &[(String, Option<String>)],
) -> Option<String> {
    let resolved = resolve_path(file);
    let mut found: Vec<(usize, isize, String)> = Vec::new();
    for (index, (name, explicit)) in targets.iter().enumerate() {
        let Some(exp) = explicit.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let target_root = resolve_path(&package_dir.join(exp));
        if resolved.starts_with(&target_root) {
            let depth = Path::new(exp).components().count();
            found.push((depth, -(index as isize), name.clone()));
        }
    }
    found.into_iter().max().map(|(_, _, name)| name)
}

fn target_for(package_dir: &Path, file: &Path) -> Option<String> {
    let manifest = package_dir.join("Package.swift");
    if !manifest.is_file() {
        return None;
    }
    let bytes = fs::read(&manifest).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    best_target(package_dir, file, &package_targets(&text))
}

fn project_name(directory: &Path) -> Option<String> {
    for ext in ["xcworkspace", "xcodeproj"] {
        let mut matches: Vec<_> = fs::read_dir(directory)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
            .collect();
        matches.sort();
        if let Some(first) = matches.first() {
            return first
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);
        }
    }
    None
}

fn xcode_name(start: &Path, stop: &Path) -> Option<String> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    let stop = resolve_path(stop);
    loop {
        if let Some(name) = project_name(&current) {
            return Some(name);
        }
        if resolve_path(&current) == stop {
            return None;
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

fn module_from_disk(path: &str, project_root: &str) -> Option<String> {
    let file_path = absolute_path(path, Some(project_root));
    let stop = resolve_path(Path::new(project_root));
    if let Some(package) = nearest_marker(&file_path, "Package.swift", &stop)
        && let Some(target) = target_for(&package, &file_path)
    {
        return Some(target);
    }
    if let Some(src_name) = sources_name(&parts(path)) {
        return Some(src_name);
    }
    xcode_name(&file_path, &stop)
}

fn module_name(path: &str, project_root: Option<&str>) -> String {
    if let Some(root) = project_root.filter(|s| !s.is_empty())
        && let Some(found) = module_from_disk(path, root)
    {
        return found;
    }
    directory_module(path, project_root)
}

pub fn functions_in_source(source: &str, path: &str, source_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, "swift") else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let module = module_name(path, source_root);
    descendants(tree.root_node())
        .into_iter()
        .filter_map(|node| recorded(data, node, &module, path))
        .collect()
}
