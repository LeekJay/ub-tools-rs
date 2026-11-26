use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tree_sitter::{Language, Node, Parser, Range, Tree};

use crate::path_util::resolve_path;

pub fn language_for(language: &str) -> Option<Language> {
    let lang: Language = match language {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "clojure" => tree_sitter_clojure_orchard::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "vue" => tree_sitter_vue_next::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

pub fn parse(source: &str, grammar: &str) -> Result<Tree, String> {
    let lang = language_for(grammar).ok_or_else(|| format!("unknown grammar: {grammar}"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("set_language failed: {e}"))?;
    parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter parse returned None".to_string())
}

pub fn parse_range(data: &[u8], grammar: &str, span: Range) -> Result<Tree, String> {
    let lang = language_for(grammar).ok_or_else(|| format!("unknown grammar: {grammar}"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("set_language failed: {e}"))?;
    parser
        .set_included_ranges(&[span])
        .map_err(|e| format!("set_included_ranges failed: {e}"))?;
    parser
        .parse(data, None)
        .ok_or_else(|| "tree-sitter parse returned None".to_string())
}

pub fn claim_name(used: &mut BTreeSet<String>, name: &str) -> String {
    if !used.contains(name) {
        let owned = name.to_string();
        used.insert(owned.clone());
        return owned;
    }
    let mut number = 2usize;
    loop {
        let candidate = format!("{name}#{number}");
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
        number += 1;
    }
}

pub fn node_text<'a>(data: &'a [u8], node: Node<'_>) -> &'a str {
    node.utf8_text(data).unwrap_or("")
}

pub fn start_line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

pub fn end_line(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 {
        start_line(node).max(end.row)
    } else {
        end.row + 1
    }
}

pub fn child_of_type<'a>(node: Node<'a>, types: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|&child| types.contains(&child.kind()))
}

pub fn child_text<'a>(data: &'a [u8], node: Node<'_>, types: &[&str]) -> Option<&'a str> {
    let ident = child_of_type(node, types)?;
    Some(node_text(data, ident))
}

pub fn strip_root(relative: &str, source_root: Option<&str>) -> String {
    let root = source_root.unwrap_or("").replace('\\', "/");
    let root = root.trim_end_matches('/');
    if !root.is_empty()
        && root != "."
        && let Some(rest) = relative.strip_prefix(&format!("{root}/"))
    {
        return rest.to_string();
    }
    if let Some(rest) = relative.strip_prefix("./") {
        return rest.to_string();
    }
    relative.to_string()
}

pub fn strip_src(relative: &str) -> String {
    if let Some(rest) = relative.strip_prefix("src/") {
        return rest.to_string();
    }
    if let Some((_, rest)) = relative.split_once("/src/") {
        return rest.to_string();
    }
    relative.to_string()
}

pub fn ancestor_names(data: &[u8], node: Node<'_>, kind: &str, name_types: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(cur) = current {
        if cur.kind() == kind
            && let Some(ident) = child_of_type(cur, name_types)
        {
            names.push(node_text(data, ident).to_string());
        }
        current = cur.parent();
    }
    names.reverse();
    names
}

pub fn descendants<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        out.push(current);
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    out
}

pub fn absolute_path(path: &str, project_root: Option<&str>) -> PathBuf {
    let file_path = Path::new(path);
    if file_path.is_absolute() {
        return file_path.to_path_buf();
    }
    if let Some(root) = project_root.filter(|s| !s.is_empty()) {
        return resolve_path(&Path::new(root).join(file_path));
    }
    resolve_path(file_path)
}

/// McCabe complexity of `node`: 1 plus each decision in the subtree.
/// Iterative walk avoids stack overflow on 1,200+ nested expressions.
pub fn complexity<F, S>(node: Node<'_>, is_decision: F, skip: S) -> usize
where
    F: Fn(Node<'_>) -> bool,
    S: Fn(Node<'_>) -> bool,
{
    let mut score = 1usize;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if skip(current) {
            continue;
        }
        if is_decision(current) {
            score += 1;
        }
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    score
}

pub fn binary_logic(node: Node<'_>) -> bool {
    if node.kind() != "binary_expression" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| matches!(child.kind(), "&&" | "||"))
}
