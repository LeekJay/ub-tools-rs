//! Normalize a concrete syntax tree the way dry4clj normalizes a Clojure form.
//!
//! Call targets, operators, and tree shape stay; local names, field names, type
//! arguments, and literals normalize away so structurally identical functions share
//! fingerprints.

use std::collections::HashMap;

use super::shape::NormNode;

/// Minimal AST node interface used by [`normalize`], decoupled from the parser crate.
pub trait SyntaxNode: Clone {
    fn id(&self) -> usize;
    fn kind(&self) -> &str;
    fn is_named(&self) -> bool;
    fn parent(&self) -> Option<Self>;
    fn children(&self) -> Vec<Self>;
    fn named_children(&self) -> Vec<Self>;
    fn text<'a>(&self, data: &'a [u8]) -> &'a str;
}

fn is_skip(kind: &str) -> bool {
    matches!(
        kind,
        "comment"
            | "line_comment"
            | "block_comment"
            | "documentation_comment"
            | "doc_comment"
            | "multiline_comment"
    )
}

fn is_identifier(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "package_identifier"
            | "simple_identifier"
    )
}

fn is_literal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string"
            | "string_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "rune_literal"
            | "character_literal"
            | "number"
            | "integer"
            | "float"
            | "decimal_integer_literal"
            | "hex_integer_literal"
            | "octal_integer_literal"
            | "binary_integer_literal"
            | "decimal_floating_point_literal"
            | "hex_floating_point_literal"
            | "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "integer_literal"
            | "true"
            | "false"
            | "null"
            | "nil"
            | "none"
            | "undefined"
            | "null_literal"
            | "boolean_literal"
            | "template_string"
            | "regex"
            | "regex_literal"
            | "line_string_literal"
            | "multi_line_string_literal"
            | "nil_literal"
            | "real_literal"
    )
}

fn is_operator(kind: &str) -> bool {
    matches!(
        kind,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "**"
            | "=="
            | "!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "==="
            | "!=="
            | "&&"
            | "||"
            | "&"
            | "|"
            | "^"
            | "<<"
            | ">>"
            | ">>>"
            | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "**="
            | "&="
            | "|="
            | "^="
            | "<<="
            | ">>="
            | ">>>="
            | "??="
            | "||="
            | "&&="
            | "!"
            | "~"
            | "++"
            | "--"
            | "and"
            | "or"
            | "not"
            | "??"
            | "?."
            | "="
            | ":="
            | "=>"
            | "?"
            | "..."
            | "..<"
    )
}

fn is_call(kind: &str) -> bool {
    matches!(
        kind,
        "call"
            | "call_expression"
            | "method_invocation"
            | "object_creation_expression"
            | "new_expression"
    )
}

fn is_attribute(kind: &str) -> bool {
    matches!(
        kind,
        "attribute"
            | "member_expression"
            | "selector_expression"
            | "field_access"
            | "field_expression"
            | "navigation_expression"
    )
}

fn is_text_symbol(kind: &str) -> bool {
    matches!(kind, "custom_operator" | "value_argument_label")
}

fn is_interpolated(kind: &str) -> bool {
    matches!(
        kind,
        "interpolation" | "interpolated_expression" | "template_substitution"
    )
}

fn is_scoped(kind: &str) -> bool {
    matches!(kind, "scoped_identifier" | "scoped_type_identifier")
}

fn is_arg_list(kind: &str) -> bool {
    matches!(kind, "argument_list" | "arguments" | "call_suffix")
}

fn is_type_args(kind: &str) -> bool {
    matches!(kind, "type_arguments" | "type_parameters")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Mode {
    Norm,
    Callee,
    Path,
}

fn sym_with_text(text: &str) -> NormNode {
    NormNode::List(vec![
        NormNode::Keyword("symbol".into()),
        NormNode::Text(text.to_string()),
    ])
}

fn is_plain_literal<N: SyntaxNode>(node: &N) -> bool {
    if !is_literal_kind(node.kind()) {
        return false;
    }
    !node
        .children()
        .into_iter()
        .any(|child| is_interpolated(child.kind()))
}

fn named_before_args<N: SyntaxNode>(node: &N) -> (Vec<N>, Option<N>) {
    let mut incoming = Vec::new();
    for child in node.named_children() {
        if is_arg_list(child.kind()) {
            return (incoming, Some(child));
        }
        incoming.push(child);
    }
    (incoming, None)
}

fn without_type_args<N: SyntaxNode>(nodes: Vec<N>) -> (Vec<N>, Vec<N>) {
    let mut rest = Vec::new();
    let mut type_args = Vec::new();
    for child in nodes {
        if is_type_args(child.kind()) {
            type_args.push(child);
        } else {
            rest.push(child);
        }
    }
    (rest, type_args)
}

fn trailing_label<N: SyntaxNode>(node: &N) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "call_suffix")
        && is_identifier(node.kind())
}

enum LeafResult {
    Pending,
    Done(Option<NormNode>),
}

fn try_leaf<N: SyntaxNode>(node: &N, mode: Mode, head: bool, data: &[u8]) -> LeafResult {
    match mode {
        Mode::Callee => {
            if is_identifier(node.kind()) {
                return LeafResult::Done(Some(sym_with_text(node.text(data))));
            }
            LeafResult::Pending
        }
        Mode::Path => LeafResult::Pending,
        Mode::Norm => {
            let kind = node.kind();
            if is_skip(kind) {
                return LeafResult::Done(None);
            }
            if is_operator(kind) {
                return LeafResult::Done(Some(sym_with_text(kind)));
            }
            if is_text_symbol(kind) || trailing_label(node) {
                return LeafResult::Done(Some(sym_with_text(node.text(data))));
            }
            if !node.is_named() {
                return LeafResult::Done(None);
            }
            if is_identifier(kind) {
                return LeafResult::Done(Some(if head {
                    sym_with_text(node.text(data))
                } else {
                    NormNode::Keyword("symbol".into())
                }));
            }
            if is_plain_literal(node) {
                return LeafResult::Done(Some(NormNode::Keyword("literal".into())));
            }
            LeafResult::Pending
        }
    }
}

fn member_name<N: SyntaxNode>(node: N) -> N {
    if node.kind() != "navigation_suffix" {
        return node;
    }
    for child in node.named_children() {
        if is_identifier(child.kind()) {
            return child;
        }
    }
    node
}

fn attr_ends<N: SyntaxNode>(node: &N) -> (Vec<N>, Option<N>) {
    let mut named = node.named_children();
    let Some(last) = named.pop() else {
        return (Vec::new(), None);
    };
    (named, Some(member_name(last)))
}

fn call_jobs<N: SyntaxNode>(node: &N) -> Vec<(N, Mode, bool)> {
    let (incoming, args) = named_before_args(node);
    let (mut rest, type_args) = without_type_args(incoming);
    let mut jobs = Vec::new();
    if let Some(callee) = rest.pop() {
        for child in rest {
            jobs.push((child, Mode::Norm, false));
        }
        jobs.push((callee, Mode::Callee, false));
    }
    for child in type_args {
        jobs.push((child, Mode::Norm, false));
    }
    if let Some(args_node) = args {
        jobs.push((args_node, Mode::Norm, false));
    }
    jobs
}

fn attr_jobs<N: SyntaxNode>(node: &N, head: bool) -> Vec<(N, Mode, bool)> {
    let (objects, name_opt) = attr_ends(node);
    let Some(name) = name_opt else {
        return Vec::new();
    };
    let mut jobs: Vec<(N, Mode, bool)> = objects
        .into_iter()
        .map(|obj| (obj, Mode::Norm, false))
        .collect();
    if !is_identifier(name.kind()) {
        jobs.push((name, if head { Mode::Callee } else { Mode::Norm }, false));
    }
    jobs
}

fn path_jobs<N: SyntaxNode>(node: &N) -> Vec<(N, Mode, bool)> {
    let mut jobs = Vec::new();
    for child in node.named_children() {
        let kind = child.kind();
        if is_identifier(kind) {
            continue;
        }
        if is_scoped(kind) || kind == "generic_type" {
            jobs.push((child, Mode::Path, false));
        } else {
            jobs.push((child, Mode::Norm, false));
        }
    }
    jobs
}

fn callee_jobs<N: SyntaxNode>(node: &N) -> Vec<(N, Mode, bool)> {
    let kind = node.kind();
    if is_attribute(kind) {
        return attr_jobs(node, true);
    }
    if is_scoped(kind) || kind == "generic_type" {
        return path_jobs(node);
    }
    vec![(node.clone(), Mode::Norm, true)]
}

fn child_jobs<N: SyntaxNode>(
    node: &N,
    mode: Mode,
    head: bool,
    unwrap_python_expr_stmt: bool,
) -> Vec<(N, Mode, bool)> {
    match mode {
        Mode::Callee => callee_jobs(node),
        Mode::Path => path_jobs(node),
        Mode::Norm => {
            let kind = node.kind();
            if kind == "parenthesized_expression"
                || (unwrap_python_expr_stmt && kind == "expression_statement")
            {
                let named = node.named_children();
                if named.len() == 1 {
                    return vec![(named.into_iter().next().unwrap(), Mode::Norm, head)];
                }
            }
            if is_call(kind) {
                return call_jobs(node);
            }
            if is_attribute(kind) {
                return attr_jobs(node, head);
            }
            node.children()
                .into_iter()
                .map(|child| (child, Mode::Norm, false))
                .collect()
        }
    }
}

type CacheKey = (usize, Mode, bool);
type Cache = HashMap<CacheKey, Option<NormNode>>;

fn cached(cache: &Cache, id: usize, mode: Mode, head: bool) -> Option<NormNode> {
    cache.get(&(id, mode, head)).cloned().flatten()
}

fn assemble_call<N: SyntaxNode>(node: &N, cache: &Cache) -> Option<NormNode> {
    let (incoming, args) = named_before_args(node);
    let (mut rest, type_args) = without_type_args(incoming);
    let mut out = vec![NormNode::Keyword(node.kind().into())];
    if let Some(callee) = rest.pop() {
        for child in rest {
            if let Some(norm) = cached(cache, child.id(), Mode::Norm, false) {
                out.push(norm);
            }
        }
        if let Some(norm) = cached(cache, callee.id(), Mode::Callee, false) {
            out.push(norm);
        }
    }
    for child in type_args {
        if let Some(norm) = cached(cache, child.id(), Mode::Norm, false) {
            out.push(norm);
        }
    }
    if let Some(args_node) = args
        && let Some(norm) = cached(cache, args_node.id(), Mode::Norm, false)
    {
        out.push(norm);
    }
    Some(NormNode::List(out))
}

fn assemble_attr<N: SyntaxNode>(
    node: &N,
    head: bool,
    cache: &Cache,
    data: &[u8],
) -> Option<NormNode> {
    let (objects, name_opt) = attr_ends(node);
    let Some(name) = name_opt else {
        return Some(NormNode::List(vec![NormNode::Keyword(node.kind().into())]));
    };
    let mut parts = vec![NormNode::Keyword(node.kind().into())];
    for obj in objects {
        if let Some(norm) = cached(cache, obj.id(), Mode::Norm, false) {
            parts.push(norm);
        }
    }
    let nested = if is_identifier(name.kind()) {
        Some(if head {
            sym_with_text(name.text(data))
        } else {
            NormNode::Keyword("symbol".into())
        })
    } else if head {
        cached(cache, name.id(), Mode::Callee, false)
    } else {
        cached(cache, name.id(), Mode::Norm, false)
    };
    if let Some(norm) = nested {
        parts.push(norm);
    }
    Some(NormNode::List(parts))
}

fn assemble_path<N: SyntaxNode>(node: &N, cache: &Cache, data: &[u8]) -> Option<NormNode> {
    let mut parts = vec![NormNode::Keyword(node.kind().into())];
    for child in node.named_children() {
        let kind = child.kind();
        if is_identifier(kind) {
            parts.push(sym_with_text(child.text(data)));
        } else if is_scoped(kind) || kind == "generic_type" {
            if let Some(norm) = cached(cache, child.id(), Mode::Path, false) {
                parts.push(norm);
            }
        } else if let Some(norm) = cached(cache, child.id(), Mode::Norm, false) {
            parts.push(norm);
        }
    }
    Some(NormNode::List(parts))
}

fn assemble_node<N: SyntaxNode>(
    node: &N,
    mode: Mode,
    head: bool,
    cache: &Cache,
    data: &[u8],
    unwrap_python_expr_stmt: bool,
) -> Option<NormNode> {
    match mode {
        Mode::Callee => {
            let kind = node.kind();
            if is_attribute(kind) {
                return assemble_attr(node, true, cache, data);
            }
            if is_scoped(kind) || kind == "generic_type" {
                return assemble_path(node, cache, data);
            }
            cached(cache, node.id(), Mode::Norm, true)
        }
        Mode::Path => assemble_path(node, cache, data),
        Mode::Norm => {
            let kind = node.kind();
            if kind == "parenthesized_expression"
                || (unwrap_python_expr_stmt && kind == "expression_statement")
            {
                let named = node.named_children();
                if named.len() == 1 {
                    return cached(cache, named[0].id(), Mode::Norm, head);
                }
            }
            if is_call(kind) {
                return assemble_call(node, cache);
            }
            if is_attribute(kind) {
                return assemble_attr(node, head, cache, data);
            }
            let mut children = vec![NormNode::Keyword(kind.into())];
            for child in node.children() {
                if let Some(norm) = cached(cache, child.id(), Mode::Norm, false) {
                    children.push(norm);
                }
            }
            Some(NormNode::List(children))
        }
    }
}

/// Normalize a concrete syntax tree into a [`NormNode`] iteratively (safe for 1,200+ depth).
pub fn normalize<N: SyntaxNode>(node: &N, data: &[u8]) -> Option<NormNode> {
    normalize_with_options(node, data, false)
}

/// Normalize a concrete syntax tree, optionally unwrapping Python `expression_statement` nodes
/// so node counts and fingerprints match `tree-sitter-language-pack==1.25.2`.
pub fn normalize_with_options<N: SyntaxNode>(
    node: &N,
    data: &[u8],
    unwrap_python_expr_stmt: bool,
) -> Option<NormNode> {
    let mut cache: Cache = HashMap::new();
    let mut stack: Vec<(N, Mode, bool, bool)> = vec![(node.clone(), Mode::Norm, false, false)];

    while let Some((current, mode, is_head, expanded)) = stack.pop() {
        let key = (current.id(), mode, is_head);
        if !expanded {
            if cache.contains_key(&key) {
                continue;
            }
            if let LeafResult::Done(leaf) = try_leaf(&current, mode, is_head, data) {
                cache.insert(key, leaf);
                continue;
            }
            let jobs = child_jobs(&current, mode, is_head, unwrap_python_expr_stmt);
            stack.push((current, mode, is_head, true));
            for (child, child_mode, child_head) in jobs.into_iter().rev() {
                stack.push((child, child_mode, child_head, false));
            }
            continue;
        }
        let built = assemble_node(
            &current,
            mode,
            is_head,
            &cache,
            data,
            unwrap_python_expr_stmt,
        );
        cache.insert(key, built);
    }

    cache.remove(&(node.id(), Mode::Norm, false)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::pr;
    use std::rc::Rc;

    #[derive(Clone)]
    struct MockTree {
        nodes: Rc<Vec<MockNode>>,
        index: usize,
    }

    struct MockNode {
        kind: &'static str,
        named: bool,
        text: &'static str,
        parent: Option<usize>,
        children: Vec<usize>,
    }

    impl SyntaxNode for MockTree {
        fn id(&self) -> usize {
            self.index
        }
        fn kind(&self) -> &str {
            self.nodes[self.index].kind
        }
        fn is_named(&self) -> bool {
            self.nodes[self.index].named
        }
        fn parent(&self) -> Option<Self> {
            self.nodes[self.index].parent.map(|index| Self {
                nodes: Rc::clone(&self.nodes),
                index,
            })
        }
        fn children(&self) -> Vec<Self> {
            self.nodes[self.index]
                .children
                .iter()
                .map(|&index| Self {
                    nodes: Rc::clone(&self.nodes),
                    index,
                })
                .collect()
        }
        fn named_children(&self) -> Vec<Self> {
            self.children()
                .into_iter()
                .filter(|c| c.is_named())
                .collect()
        }
        fn text<'a>(&self, _data: &'a [u8]) -> &'a str {
            self.nodes[self.index].text
        }
    }

    fn build_parenthesized_call(
        callee: &'static str,
        arg: &'static str,
        parenthesize: bool,
    ) -> MockTree {
        let mut nodes = vec![
            MockNode {
                kind: "call",
                named: true,
                text: "",
                parent: None,
                children: vec![],
            },
            MockNode {
                kind: "identifier",
                named: true,
                text: callee,
                parent: None,
                children: vec![],
            },
            MockNode {
                kind: "argument_list",
                named: true,
                text: "",
                parent: Some(0),
                children: vec![3],
            },
            MockNode {
                kind: "identifier",
                named: true,
                text: arg,
                parent: Some(2),
                children: vec![],
            },
        ];
        if parenthesize {
            nodes.push(MockNode {
                kind: "parenthesized_expression",
                named: true,
                text: "",
                parent: Some(0),
                children: vec![1],
            });
            nodes[1].parent = Some(4);
            nodes[0].children = vec![4, 2];
        } else {
            nodes[1].parent = Some(0);
            nodes[0].children = vec![1, 2];
        }
        MockTree {
            nodes: Rc::new(nodes),
            index: 0,
        }
    }

    #[test]
    fn parenthesized_callee_matches_bare_call_and_erases_arg_ident() {
        let bare = normalize(&build_parenthesized_call("len", "xs", false), b"").unwrap();
        let wrapped = normalize(&build_parenthesized_call("len", "items", true), b"").unwrap();
        assert_eq!(pr(&bare), pr(&wrapped));
        assert!(pr(&bare).contains("\"len\""));
        assert!(!pr(&bare).contains("xs"));
    }
}
