use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::{Node, Range};

use crate::languages::treesitter::{
    child_of_type, child_text, claim_name, descendants, node_text, parse, parse_range, strip_root,
    strip_src,
};
use crate::languages::typescript::{
    WRAPPERS, append_function, call_callee, extract_tree_functions, has_body, inside_function,
    literal_text, property_name, unwrap_expression,
};
use crate::model::Function;

#[derive(Debug, Clone)]
pub struct ScriptBlock {
    pub grammar: &'static str,
    pub is_setup: bool,
    pub span: Range,
}

struct Target<'a> {
    data: &'a [u8],
    module: String,
    path: &'a str,
}

const OPTIONS_SKIP_KEYS: &[&str] = &[
    "props",
    "emits",
    "components",
    "directives",
    "provide",
    "inject",
];
const CALLBACKS: &[&str] = &["arrow_function", "function_expression"];
const EFFECT_HOOKS: &[&str] = &[
    "watchEffect",
    "watchPostEffect",
    "watchSyncEffect",
    "onBeforeMount",
    "onMounted",
    "onBeforeUpdate",
    "onUpdated",
    "onBeforeUnmount",
    "onUnmounted",
    "onActivated",
    "onDeactivated",
    "onErrorCaptured",
    "onRenderTracked",
    "onRenderTriggered",
    "onServerPrefetch",
];
const WATCH_SIMPLE: &[&str] = &["identifier", "member_expression"];
const MAX_WATCH_LABEL: usize = 32;

fn script_grammar(lang: &str) -> Option<&'static str> {
    match lang {
        "" | "js" | "jsx" | "mjs" | "cjs" | "javascript" => Some("javascript"),
        "ts" | "mts" | "cts" | "typescript" => Some("typescript"),
        "tsx" => Some("tsx"),
        _ => None,
    }
}

fn attribute_value(data: &[u8], attr: Node<'_>) -> String {
    if let Some(quoted) = child_of_type(attr, &["quoted_attribute_value"]) {
        return child_text(data, quoted, &["attribute_value"])
            .unwrap_or("")
            .to_string();
    }
    child_text(data, attr, &["attribute_value"])
        .unwrap_or("")
        .to_string()
}

fn start_attributes(data: &[u8], start_tag: Node<'_>) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    let mut cursor = start_tag.walk();
    for child in start_tag.children(&mut cursor) {
        if child.kind() != "attribute" {
            continue;
        }
        if let Some(name) = child_text(data, child, &["attribute_name"]) {
            attrs.insert(name.to_ascii_lowercase(), attribute_value(data, child));
        }
    }
    attrs
}

fn script_block(data: &[u8], element: Node<'_>) -> Option<ScriptBlock> {
    let start_tag = child_of_type(element, &["start_tag"])?;
    let raw_text = child_of_type(element, &["raw_text"])?;
    let slice = &data[raw_text.start_byte()..raw_text.end_byte()];
    if String::from_utf8_lossy(slice).trim().is_empty() {
        return None;
    }
    let attrs = start_attributes(data, start_tag);
    if attrs.contains_key("src") {
        return None;
    }
    let lang = attrs
        .get("lang")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let grammar = script_grammar(&lang)?;
    Some(ScriptBlock {
        grammar,
        is_setup: attrs.contains_key("setup"),
        span: raw_text.range(),
    })
}

pub fn extract_script_blocks(source: &str) -> (Vec<u8>, Vec<ScriptBlock>) {
    let data = source.as_bytes().to_vec();
    let Ok(tree) = parse(source, "vue") else {
        return (data, Vec::new());
    };
    let mut blocks = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "script_element" {
            continue;
        }
        if let Some(block) = script_block(&data, child) {
            blocks.push(block);
        }
    }
    (data, blocks)
}

fn vue_namespace(path: &str, source_root: Option<&str>) -> String {
    let normalized = path.replace('\\', "/");
    let relative = strip_src(&strip_root(&normalized, source_root));
    let trimmed = relative.strip_suffix(".vue").unwrap_or(&relative);
    trimmed.replace('/', ".")
}

fn record(
    found: &mut Vec<(usize, Function)>,
    span_node: Node<'_>,
    body_node: Node<'_>,
    target: &Target<'_>,
    name: &str,
) {
    append_function(
        found,
        span_node,
        body_node,
        target.data,
        name,
        target.module.clone(),
        target.path,
        "vue",
        false,
    );
}

fn is_callable(node: Option<Node<'_>>) -> bool {
    let Some(n) = node else {
        return false;
    };
    if !CALLBACKS.contains(&n.kind()) {
        return false;
    }
    n.kind() == "arrow_function" || has_body(n)
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.is_named() && c.kind() != "comment")
        .collect()
}

fn call_args(call: Node<'_>) -> Vec<Node<'_>> {
    child_of_type(call, &["arguments"])
        .map(named_children)
        .unwrap_or_default()
}

fn member_name(data: &[u8], member: Node<'_>) -> Option<String> {
    if let Some(ident) = child_of_type(member, &["property_identifier", "identifier"]) {
        return Some(node_text(data, ident).to_string());
    }
    let text_node = child_of_type(member, &["string"])?;
    literal_text(data, text_node)
}

fn named_members<'a>(data: &[u8], obj: Node<'a>) -> Vec<(Node<'a>, String)> {
    let mut out = Vec::new();
    let mut cursor = obj.walk();
    for member in obj.children(&mut cursor) {
        if !matches!(member.kind(), "method_definition" | "pair") {
            continue;
        }
        if let Some(key) = member_name(data, member) {
            out.push((member, key));
        }
    }
    out
}

fn pair_value<'a>(member: Node<'a>, types: &[&str]) -> Option<Node<'a>> {
    if member.kind() != "pair" {
        return None;
    }
    let value = unwrap_expression(member.child_by_field_name("value"))?;
    if !types.is_empty() && !types.contains(&value.kind()) {
        return None;
    }
    Some(value)
}

fn callback_body(member: Node<'_>) -> Option<Node<'_>> {
    if member.kind() == "method_definition" {
        return if has_body(member) { Some(member) } else { None };
    }
    let value = pair_value(member, &[])?;
    if is_callable(Some(value)) {
        Some(value)
    } else {
        None
    }
}

fn append_member_fn(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    member: Node<'_>,
    name: &str,
) -> bool {
    let Some(body) = callback_body(member) else {
        return false;
    };
    record(found, member, body, target, name);
    true
}

fn extract_accessors(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    obj: Node<'_>,
    prefix: &str,
) {
    for (member, key) in named_members(target.data, obj) {
        if matches!(key.as_str(), "get" | "set") {
            append_member_fn(found, target, member, &format!("{prefix}.{key}"));
        }
    }
}

fn extract_methods(found: &mut Vec<(usize, Function)>, target: &Target<'_>, obj: Node<'_>) {
    for (member, key) in named_members(target.data, obj) {
        append_member_fn(found, target, member, &key);
    }
}

fn extract_computed_object(found: &mut Vec<(usize, Function)>, target: &Target<'_>, obj: Node<'_>) {
    for (member, key) in named_members(target.data, obj) {
        if append_member_fn(found, target, member, &key) {
            continue;
        }
        if let Some(value) = pair_value(member, &["object"]) {
            extract_accessors(found, target, value, &key);
        }
    }
}

fn extract_watch_handler(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    obj: Node<'_>,
    name: &str,
) {
    for (member, key) in named_members(target.data, obj) {
        if key == "handler" {
            append_member_fn(found, target, member, name);
        }
    }
}

fn extract_watch_array(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    array: Node<'_>,
    name: &str,
) {
    for child in named_children(array) {
        let Some(item) = unwrap_expression(Some(child)) else {
            continue;
        };
        if is_callable(Some(item)) {
            record(found, item, item, target, name);
        } else if item.kind() == "object" {
            extract_watch_handler(found, target, item, name);
        }
    }
}

fn extract_watch_object(found: &mut Vec<(usize, Function)>, target: &Target<'_>, obj: Node<'_>) {
    for (member, key) in named_members(target.data, obj) {
        let name = format!("watch({key})");
        if append_member_fn(found, target, member, &name) {
            continue;
        }
        let Some(value) = pair_value(member, &["object", "array"]) else {
            continue;
        };
        if value.kind() == "object" {
            extract_watch_handler(found, target, value, &name);
        } else {
            extract_watch_array(found, target, value, &name);
        }
    }
}

fn extract_options_object(found: &mut Vec<(usize, Function)>, target: &Target<'_>, obj: Node<'_>) {
    for (member, key) in named_members(target.data, obj) {
        if OPTIONS_SKIP_KEYS.contains(&key.as_str())
            || append_member_fn(found, target, member, &key)
        {
            continue;
        }
        if !matches!(key.as_str(), "methods" | "computed" | "watch") {
            continue;
        }
        let Some(value) = pair_value(member, &["object"]) else {
            continue;
        };
        match key.as_str() {
            "methods" => extract_methods(found, target, value),
            "computed" => extract_computed_object(found, target, value),
            "watch" => extract_watch_object(found, target, value),
            _ => {}
        }
    }
}

fn callee_parts<'a>(data: &'a [u8], call: Node<'a>) -> Option<(Option<&'a str>, &'a str)> {
    let callee = unwrap_expression(call_callee(call))?;
    if callee.kind() == "identifier" {
        return Some((None, node_text(data, callee)));
    }
    if callee.kind() != "member_expression" {
        return None;
    }
    let obj = unwrap_expression(callee.child_by_field_name("object"))?;
    let prop = property_name(data, Some(callee))?;
    if obj.kind() == "identifier" {
        Some((Some(node_text(data, obj)), prop))
    } else {
        None
    }
}

fn is_component_call(data: &[u8], call: Node<'_>) -> bool {
    matches!(
        callee_parts(data, call),
        Some((None, "defineComponent")) | Some((Some("Vue" | "vue"), "defineComponent" | "extend"))
    )
}

fn extract_component_call(found: &mut Vec<(usize, Function)>, target: &Target<'_>, call: Node<'_>) {
    let args = call_args(call);
    let Some(&first) = args.first() else {
        return;
    };
    let Some(arg) = unwrap_expression(Some(first)) else {
        return;
    };
    if arg.kind() == "object" {
        extract_options_object(found, target, arg);
    } else if is_callable(Some(arg)) {
        record(found, arg, arg, target, "setup");
    }
}

fn export_default_expr(stmt: Node<'_>) -> Option<Node<'_>> {
    let mut seen_default = false;
    let mut cursor = stmt.walk();
    for child in stmt.children(&mut cursor) {
        if child.kind() == "default" {
            seen_default = true;
        } else if seen_default && child.is_named() && child.kind() != "comment" {
            return unwrap_expression(Some(child));
        }
    }
    None
}

fn is_anonymous_callable(node: Node<'_>) -> bool {
    if node.kind() == "arrow_function" {
        return true;
    }
    node.kind() == "function_expression"
        && child_of_type(node, &["identifier"]).is_none()
        && has_body(node)
}

fn extract_export_default(found: &mut Vec<(usize, Function)>, target: &Target<'_>, stmt: Node<'_>) {
    let Some(exported) = export_default_expr(stmt) else {
        return;
    };
    if exported.kind() == "object" {
        extract_options_object(found, target, exported);
    } else if exported.kind() == "call_expression" && is_component_call(target.data, exported) {
        extract_component_call(found, target, exported);
    } else if is_anonymous_callable(exported) {
        record(found, exported, exported, target, "default");
    }
}

fn extract_declaration_components(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    stmt: Node<'_>,
) {
    let mut cursor = stmt.walk();
    for declarator in stmt.children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        let Some(value) = unwrap_expression(declarator.child_by_field_name("value")) else {
            continue;
        };
        if value.kind() == "call_expression" && is_component_call(target.data, value) {
            extract_component_call(found, target, value);
        }
    }
}

fn extract_expression_component(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    stmt: Node<'_>,
) {
    let mut cursor = stmt.walk();
    let expr = stmt
        .children(&mut cursor)
        .find(|c| c.is_named() && c.kind() != "comment");
    if let Some(value) = unwrap_expression(expr)
        && value.kind() == "call_expression"
        && is_component_call(target.data, value)
    {
        extract_component_call(found, target, value);
    }
}

fn extract_options(found: &mut Vec<(usize, Function)>, target: &Target<'_>, root: Node<'_>) {
    let mut cursor = root.walk();
    for stmt in root.children(&mut cursor) {
        match stmt.kind() {
            "export_statement" => {
                extract_export_default(found, target, stmt);
                if let Some(decl) = child_of_type(stmt, &["lexical_declaration"]) {
                    extract_declaration_components(found, target, decl);
                }
            }
            "lexical_declaration" => extract_declaration_components(found, target, stmt),
            "expression_statement" => extract_expression_component(found, target, stmt),
            _ => {}
        }
    }
}

fn composition_callee<'a>(data: &'a [u8], call: Node<'a>) -> Option<&'a str> {
    let (receiver, name) = callee_parts(data, call)?;
    if matches!(receiver, None | Some("Vue" | "vue")) {
        Some(name)
    } else {
        None
    }
}

fn computed_declarator(call: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let mut current = call.parent();
    while let Some(cur) = current {
        if !WRAPPERS.contains(&cur.kind()) {
            break;
        }
        current = cur.parent();
    }
    let cur = current?;
    if cur.kind() != "variable_declarator" {
        return None;
    }
    let ident = child_of_type(cur, &["identifier"])?;
    Some((cur, ident))
}

fn extract_computed_call(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    call: Node<'_>,
    arg0: Node<'_>,
) {
    let Some(arg) = unwrap_expression(Some(arg0)) else {
        return;
    };
    let decl_ident = computed_declarator(call);
    let name = decl_ident
        .map(|(_, ident)| node_text(target.data, ident))
        .unwrap_or("computed");
    if is_callable(Some(arg)) {
        let span_node = decl_ident.map(|(decl, _)| decl).unwrap_or(arg);
        record(found, span_node, arg, target, name);
    } else if arg.kind() == "object" {
        extract_accessors(found, target, arg, name);
    }
}

fn getter_arrow_body(expr: Node<'_>) -> Option<Node<'_>> {
    let params = child_of_type(expr, &["formal_parameters"])?;
    let mut cursor = params.walk();
    if params.children(&mut cursor).any(|c| c.is_named()) {
        return None;
    }
    let body = unwrap_expression(expr.child_by_field_name("body"))?;
    if body.kind() == "statement_block" {
        None
    } else {
        Some(body)
    }
}

fn single_line_text<'a>(data: &'a [u8], node: Node<'_>) -> Option<&'a str> {
    let text = node_text(data, node);
    if text.contains('\n') || text.contains('\r') {
        None
    } else {
        Some(text)
    }
}

fn bounded_single_line<'a>(data: &'a [u8], node: Node<'_>) -> Option<&'a str> {
    let text = single_line_text(data, node)?;
    if text.chars().count() > MAX_WATCH_LABEL {
        None
    } else {
        Some(text)
    }
}

fn watch_atom(data: &[u8], node: Node<'_>) -> Option<String> {
    let mut expr = unwrap_expression(Some(node))?;
    if expr.kind() == "arrow_function" {
        expr = getter_arrow_body(expr)?;
    }
    if !WATCH_SIMPLE.contains(&expr.kind()) {
        return None;
    }
    bounded_single_line(data, expr).map(str::to_string)
}

fn array_watch_label(data: &[u8], array: Node<'_>) -> Option<String> {
    single_line_text(data, array)?;
    let children = named_children(array);
    let mut atoms = Vec::new();
    for child in &children {
        atoms.push(watch_atom(data, *child)?);
    }
    if atoms.is_empty() {
        return None;
    }
    let joined = format!("[{}]", atoms.join(", "));
    if joined.chars().count() <= MAX_WATCH_LABEL {
        Some(joined)
    } else {
        None
    }
}

fn watch_label(data: &[u8], arg0: Node<'_>) -> Option<String> {
    let expr = unwrap_expression(Some(arg0))?;
    if expr.kind() == "array" {
        array_watch_label(data, expr)
    } else {
        watch_atom(data, expr)
    }
}

fn extract_watch_call(found: &mut Vec<(usize, Function)>, target: &Target<'_>, args: &[Node<'_>]) {
    if args.len() < 2 {
        return;
    }
    let Some(cb) = unwrap_expression(Some(args[1])) else {
        return;
    };
    if !is_callable(Some(cb)) {
        return;
    }
    let name = match watch_label(target.data, args[0]) {
        Some(label) => format!("watch({label})"),
        None => "watch".to_string(),
    };
    record(found, cb, cb, target, &name);
}

fn extract_effect_call(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    hook: &str,
    arg0: Node<'_>,
) {
    if let Some(cb) = unwrap_expression(Some(arg0))
        && is_callable(Some(cb))
    {
        record(found, cb, cb, target, hook);
    }
}

fn extract_composition_call(
    found: &mut Vec<(usize, Function)>,
    target: &Target<'_>,
    call: Node<'_>,
) {
    let Some(callee_name) = composition_callee(target.data, call) else {
        return;
    };
    let args = call_args(call);
    if args.is_empty() {
        return;
    }
    if callee_name == "computed" {
        extract_computed_call(found, target, call, args[0]);
    } else if callee_name == "watch" {
        extract_watch_call(found, target, &args);
    } else if EFFECT_HOOKS.contains(&callee_name) {
        extract_effect_call(found, target, callee_name, args[0]);
    }
}

fn extract_composition(found: &mut Vec<(usize, Function)>, target: &Target<'_>, root: Node<'_>) {
    for node in descendants(root) {
        if node.kind() == "call_expression" && !inside_function(node) {
            extract_composition_call(found, target, node);
        }
    }
}

fn dedupe_names(mut found: Vec<(usize, Function)>) -> Vec<Function> {
    found.sort_by_key(|(start, f)| (*start, f.end_byte.unwrap_or(0)));
    let mut seen_spans = BTreeSet::new();
    let mut used_by_ns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unique = Vec::new();
    for (_start, mut func) in found {
        let span = (func.start_byte, func.end_byte);
        if !seen_spans.insert(span) {
            continue;
        }
        let used = used_by_ns.entry(func.namespace.clone()).or_default();
        let chosen = claim_name(used, &func.name);
        func.name = chosen;
        unique.push(func);
    }
    unique
}

pub fn functions_in_source(source: &str, path: &str, source_root: Option<&str>) -> Vec<Function> {
    let (data, blocks) = extract_script_blocks(source);
    if blocks.is_empty() {
        return Vec::new();
    }
    let target = Target {
        data: &data,
        module: vue_namespace(path, source_root),
        path,
    };
    let mut found = Vec::new();
    for block in blocks {
        let Ok(tree) = parse_range(&data, block.grammar, block.span) else {
            continue;
        };
        let root = tree.root_node();
        found.extend(extract_tree_functions(
            &data,
            root,
            &target.module,
            path,
            "vue",
            false,
        ));
        extract_options(&mut found, &target, root);
        extract_composition(&mut found, &target, root);
    }
    dedupe_names(found)
}
