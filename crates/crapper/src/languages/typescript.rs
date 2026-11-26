use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::languages::treesitter::{
    binary_logic, child_of_type, claim_name, complexity, descendants, end_line, node_text, parse,
    start_line, strip_root, strip_src,
};
use crate::model::Function;

const DECISIONS: &[&str] = &[
    "if_statement",
    "for_statement",
    "for_in_statement",
    "while_statement",
    "do_statement",
    "catch_clause",
    "ternary_expression",
    "switch_case",
    "switch_default",
];

const SKIP: &[&str] = &[
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
];

const FUNCTION_TYPES: &[&str] = &[
    "function_declaration",
    "method_definition",
    "arrow_function",
    "function_expression",
];

const ROUTE_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "all", "use",
];

const CALLBACKS: &[&str] = &["arrow_function", "function_expression"];
const JS_SUFFIXES: &[&str] = &[".jsx", ".mjs", ".cjs", ".js"];

pub(crate) const WRAPPERS: &[&str] = &[
    "parenthesized_expression",
    "as_expression",
    "satisfies_expression",
    "type_assertion",
];

const WRAPPER_SKIP: &[&str] = &["type_arguments", "comment"];

fn is_nullish(node: Node<'_>) -> bool {
    if node.kind() == "binary_expression" {
        let mut cursor = node.walk();
        return node.children(&mut cursor).any(|c| c.kind() == "??");
    }
    if node.kind() == "optional_chain" {
        return true;
    }
    if node.kind() != "?." {
        return false;
    }
    node.parent().is_none_or(|p| p.kind() != "optional_chain")
}

fn is_decision(node: Node<'_>) -> bool {
    DECISIONS.contains(&node.kind()) || binary_logic(node) || is_nullish(node)
}

fn skip_type(node: Node<'_>) -> bool {
    SKIP.contains(&node.kind())
}

fn grammar(path: &str) -> &'static str {
    if path.ends_with(".tsx") {
        return "tsx";
    }
    if JS_SUFFIXES.iter().any(|s| path.ends_with(s)) {
        return "javascript";
    }
    "typescript"
}

fn strip_script_suffix(relative: &str) -> &str {
    for suffix in [".tsx", ".mts", ".cts", ".jsx", ".mjs", ".cjs", ".ts", ".js"] {
        if let Some(stripped) = relative.strip_suffix(suffix) {
            return stripped;
        }
    }
    relative
}

fn module_namespace(path: &str, source_root: Option<&str>) -> String {
    let relative = path.replace('\\', "/");
    let relative = strip_root(&relative, source_root);
    let relative = strip_src(&relative);
    strip_script_suffix(&relative).replace('/', ".")
}

pub(crate) fn inside_function(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(cur) = current {
        if FUNCTION_TYPES.contains(&cur.kind()) {
            return true;
        }
        current = cur.parent();
    }
    false
}

fn class_names(data: &[u8], node: Node<'_>) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(cur) = current {
        if matches!(
            cur.kind(),
            "class_declaration" | "abstract_class_declaration"
        ) && let Some(ident) = child_of_type(cur, &["type_identifier", "identifier"])
        {
            names.push(node_text(data, ident).to_string());
        }
        current = cur.parent();
    }
    names.reverse();
    names
}

pub(crate) fn has_body(node: Node<'_>) -> bool {
    child_of_type(node, &["statement_block"]).is_some()
}

fn unwrapped_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|c| c.is_named() && !WRAPPER_SKIP.contains(&c.kind()))
}

pub(crate) fn unwrap_expression(node: Option<Node<'_>>) -> Option<Node<'_>> {
    let mut current = node;
    while let Some(cur) = current {
        if !WRAPPERS.contains(&cur.kind()) {
            break;
        }
        current = unwrapped_child(cur);
    }
    current
}

pub(crate) fn call_callee(call: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = call.walk();
    call.children(&mut cursor)
        .find(|c| !matches!(c.kind(), "arguments" | "type_arguments" | "comment"))
}

pub(crate) fn property_name<'a>(data: &'a [u8], node: Option<Node<'a>>) -> Option<&'a str> {
    let n = node?;
    if n.kind() != "member_expression" {
        return None;
    }
    let ident = child_of_type(n, &["property_identifier"])?;
    Some(node_text(data, ident))
}

fn route_method(data: &[u8], call: Node<'_>) -> Option<String> {
    let name = property_name(data, call_callee(call))?;
    let lowered = name.to_ascii_lowercase();
    if ROUTE_METHODS.contains(&lowered.as_str()) {
        Some(lowered)
    } else {
        None
    }
}

pub(crate) fn literal_text(data: &[u8], node: Node<'_>) -> Option<String> {
    if node.kind() == "string" {
        let mut cursor = node.walk();
        let out: String = node
            .children(&mut cursor)
            .filter(|c| c.kind() == "string_fragment")
            .map(|c| node_text(data, c))
            .collect();
        return Some(out);
    }
    if node.kind() == "template_string" {
        let text = node_text(data, node);
        if text.len() >= 2 && text.starts_with('`') && text.ends_with('`') {
            return Some(text[1..text.len() - 1].to_string());
        }
    }
    None
}

fn string_argument(data: &[u8], call: Node<'_>) -> Option<String> {
    let arguments = child_of_type(call, &["arguments"])?;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        let Some(expr) = unwrap_expression(Some(child)) else {
            continue;
        };
        if let Some(text) = literal_text(data, expr) {
            return Some(text);
        }
    }
    None
}

fn path_from_route(data: &[u8], node: Option<Node<'_>>) -> Option<String> {
    let mut current = node;
    while let Some(cur) = current {
        if cur.kind() == "member_expression" {
            let mut cursor = cur.walk();
            current = cur.children(&mut cursor).next();
            continue;
        }
        if cur.kind() != "call_expression" {
            return None;
        }
        let callee = call_callee(cur);
        if property_name(data, callee) == Some("route") {
            return string_argument(data, cur);
        }
        current = callee;
    }
    None
}

fn callbacks(call: Node<'_>) -> Vec<Node<'_>> {
    let Some(arguments) = child_of_type(call, &["arguments"]) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        if let Some(expr) = unwrap_expression(Some(child))
            && CALLBACKS.contains(&expr.kind())
        {
            found.push(expr);
        }
    }
    found
}

fn argument_list_parent(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(cur) = current {
        if !WRAPPERS.contains(&cur.kind()) {
            break;
        }
        current = cur.parent();
    }
    let cur = current?;
    if cur.kind() == "arguments" {
        cur.parent()
    } else {
        None
    }
}

fn is_route_callback(data: &[u8], node: Node<'_>) -> bool {
    if !CALLBACKS.contains(&node.kind()) {
        return false;
    }
    let Some(call) = argument_list_parent(node) else {
        return false;
    };
    if call.kind() != "call_expression" {
        return false;
    }
    route_method(data, call).is_some()
}

pub(crate) fn script_complexity(data: &[u8], node: Node<'_>, include_routes: bool) -> usize {
    let root_id = node.id();
    complexity(node, is_decision, |current| {
        if current.id() == root_id {
            return false;
        }
        if skip_type(current) {
            return true;
        }
        include_routes && is_route_callback(data, current)
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors typescript.py append_function helper"
)]
pub(crate) fn append_function(
    found: &mut Vec<(usize, Function)>,
    span_node: Node<'_>,
    body_node: Node<'_>,
    data: &[u8],
    name: impl Into<String>,
    namespace: impl Into<String>,
    path: &str,
    language: &str,
    include_routes: bool,
) {
    let mut func = Function::new(
        name,
        namespace,
        script_complexity(data, body_node, include_routes),
        start_line(span_node),
        end_line(span_node),
        path,
        language,
    );
    func.start_byte = Some(span_node.start_byte());
    func.end_byte = Some(span_node.end_byte());
    found.push((span_node.start_byte(), func));
}

fn append_named_function(
    found: &mut Vec<(usize, Function)>,
    data: &[u8],
    node: Node<'_>,
    module: &str,
    path: &str,
    language: &str,
    include_routes: bool,
) {
    if inside_function(node) || !has_body(node) {
        return;
    }
    let Some(ident) = child_of_type(node, &["identifier"]) else {
        return;
    };
    append_function(
        found,
        node,
        node,
        data,
        node_text(data, ident),
        module,
        path,
        language,
        include_routes,
    );
}

fn append_method(
    found: &mut Vec<(usize, Function)>,
    data: &[u8],
    node: Node<'_>,
    module: &str,
    path: &str,
    language: &str,
    include_routes: bool,
) {
    if !has_body(node) {
        return;
    }
    let Some(ident) = child_of_type(node, &["property_identifier", "identifier"]) else {
        return;
    };
    let classes = class_names(data, node);
    if classes.is_empty() {
        return;
    }
    append_function(
        found,
        node,
        node,
        data,
        node_text(data, ident),
        format!("{module}.{}", classes.join(".")),
        path,
        language,
        include_routes,
    );
}

fn declarator_callback(declarator: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    if declarator.kind() != "variable_declarator" {
        return None;
    }
    let ident = child_of_type(declarator, &["identifier"])?;
    let value = unwrap_expression(declarator.child_by_field_name("value"))?;
    if !CALLBACKS.contains(&value.kind()) {
        return None;
    }
    Some((ident, value))
}

fn append_arrows(
    found: &mut Vec<(usize, Function)>,
    data: &[u8],
    node: Node<'_>,
    module: &str,
    path: &str,
    language: &str,
    include_routes: bool,
) {
    if inside_function(node) {
        return;
    }
    let mut cursor = node.walk();
    for declarator in node.children(&mut cursor) {
        let Some((ident, value)) = declarator_callback(declarator) else {
            continue;
        };
        append_function(
            found,
            declarator,
            value,
            data,
            node_text(data, ident),
            module,
            path,
            language,
            include_routes,
        );
    }
}

fn route_label(method: &str, route: Option<&str>) -> String {
    let label = method.to_ascii_uppercase();
    match route.filter(|s| !s.is_empty()) {
        Some(r) => format!("{label} {r}"),
        None => label,
    }
}

struct RouteEntry<'a> {
    start_byte: usize,
    callback: Node<'a>,
    method: String,
    route: Option<String>,
}

fn collect_routes<'a>(routes: &mut Vec<RouteEntry<'a>>, data: &[u8], call: Node<'a>) {
    let Some(method) = route_method(data, call) else {
        return;
    };
    let cbs = callbacks(call);
    if cbs.is_empty() {
        return;
    }
    let route = string_argument(data, call)
        .filter(|s| !s.is_empty())
        .or_else(|| path_from_route(data, call_callee(call)));
    for callback in cbs {
        routes.push(RouteEntry {
            start_byte: callback.start_byte(),
            callback,
            method: method.clone(),
            route: route.clone(),
        });
    }
}

fn append_routes(
    found: &mut Vec<(usize, Function)>,
    mut routes: Vec<RouteEntry<'_>>,
    data: &[u8],
    module: &str,
    path: &str,
    language: &str,
) {
    routes.sort_by_key(|r| r.start_byte);
    let mut used = BTreeSet::new();
    for entry in routes {
        let name = claim_name(
            &mut used,
            &route_label(&entry.method, entry.route.as_deref()),
        );
        append_function(
            found,
            entry.callback,
            entry.callback,
            data,
            name,
            module,
            path,
            language,
            true,
        );
    }
}

pub(crate) fn extract_tree_functions(
    data: &[u8],
    root: Node<'_>,
    module: &str,
    path: &str,
    language: &str,
    include_routes: bool,
) -> Vec<(usize, Function)> {
    let mut found = Vec::new();
    let mut routes = Vec::new();
    for node in descendants(root) {
        match node.kind() {
            "function_declaration" => append_named_function(
                &mut found,
                data,
                node,
                module,
                path,
                language,
                include_routes,
            ),
            "method_definition" => append_method(
                &mut found,
                data,
                node,
                module,
                path,
                language,
                include_routes,
            ),
            "lexical_declaration" => append_arrows(
                &mut found,
                data,
                node,
                module,
                path,
                language,
                include_routes,
            ),
            "call_expression" if include_routes => collect_routes(&mut routes, data, node),
            _ => {}
        }
    }
    append_routes(&mut found, routes, data, module, path, language);
    found
}

pub fn functions_in_source(source: &str, path: &str, source_root: Option<&str>) -> Vec<Function> {
    let Ok(tree) = parse(source, grammar(path)) else {
        return Vec::new();
    };
    let data = source.as_bytes();
    let module = module_namespace(path, source_root);
    let mut found =
        extract_tree_functions(data, tree.root_node(), &module, path, "typescript", true);
    found.sort_by_key(|(start, _)| *start);
    found.into_iter().map(|(_, f)| f).collect()
}
