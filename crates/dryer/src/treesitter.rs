//! Shared tree-sitter parsing and node traversal helpers.

use tree_sitter::{Language, Node, Parser, Range, Tree};

use super::astnorm::SyntaxNode;

pub fn language_for(name: &str) -> Option<Language> {
    let lang = match name {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "vue" => tree_sitter_vue_next::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

pub fn parser_for(language: &str) -> Result<Parser, String> {
    let lang = language_for(language).ok_or_else(|| format!("unknown grammar: {language}"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|err| format!("set_language failed for {language}: {err}"))?;
    Ok(parser)
}

pub fn parse(source: &str, language: &str) -> Result<(Vec<u8>, Tree), String> {
    let data = source.as_bytes().to_vec();
    let mut parser = parser_for(language)?;
    let tree = parser
        .parse(&data, None)
        .ok_or_else(|| format!("tree-sitter parse returned None for {language}"))?;
    Ok((data, tree))
}

pub fn parse_range(data: &[u8], language: &str, span: Range) -> Result<Tree, String> {
    let mut parser = parser_for(language)?;
    parser
        .set_included_ranges(&[span])
        .map_err(|err| format!("set_included_ranges failed for {language}: {err}"))?;
    parser
        .parse(data, None)
        .ok_or_else(|| format!("tree-sitter parse_range returned None for {language}"))
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

/// Iterative DFS pre-order traversal (safe for 1,200+ deep expressions).
pub fn descendants<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        out.push(current);
        let mut cursor = current.walk();
        let children: Vec<Node<'a>> = current.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    out
}

pub fn child_of_type<'a>(node: Node<'a>, types: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| types.contains(&child.kind()))
}

impl<'tree> SyntaxNode for Node<'tree> {
    fn id(&self) -> usize {
        Node::id(self)
    }

    fn kind(&self) -> &str {
        Node::kind(self)
    }

    fn is_named(&self) -> bool {
        Node::is_named(self)
    }

    fn parent(&self) -> Option<Self> {
        Node::parent(self)
    }

    fn children(&self) -> Vec<Self> {
        let mut cursor = self.walk();
        Node::children(self, &mut cursor).collect()
    }

    fn named_children(&self) -> Vec<Self> {
        let mut cursor = self.walk();
        Node::named_children(self, &mut cursor).collect()
    }

    fn text<'a>(&self, data: &'a [u8]) -> &'a str {
        node_text(data, *self)
    }
}
