//! Extract `<script>` blocks and candidate functions from Vue Single File Components.

use std::collections::BTreeMap;

use tree_sitter::{Node, Range};

use super::treesitter::{child_of_type, node_text, parse, parse_range};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptGrammar {
    Javascript,
    Typescript,
    Tsx,
}

impl ScriptGrammar {
    pub fn as_str(self) -> &'static str {
        match self {
            ScriptGrammar::Javascript => "javascript",
            ScriptGrammar::Typescript => "typescript",
            ScriptGrammar::Tsx => "tsx",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptBlock {
    pub grammar: ScriptGrammar,
    pub is_setup: bool,
    pub span: Range,
}

fn script_grammar(lang: &str) -> Option<ScriptGrammar> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "" | "js" | "jsx" | "mjs" | "cjs" | "javascript" => Some(ScriptGrammar::Javascript),
        "ts" | "mts" | "cts" | "typescript" => Some(ScriptGrammar::Typescript),
        "tsx" => Some(ScriptGrammar::Tsx),
        _ => None,
    }
}

fn attr_value(data: &[u8], attr: Node<'_>) -> String {
    let node = if let Some(quoted) = child_of_type(attr, &["quoted_attribute_value"]) {
        child_of_type(quoted, &["attribute_value"])
    } else {
        child_of_type(attr, &["attribute_value"])
    };
    node.map(|n| node_text(data, n).to_string())
        .unwrap_or_default()
}

fn tag_attrs(data: &[u8], start_tag: Node<'_>) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    let mut cursor = start_tag.walk();
    for child in start_tag.children(&mut cursor) {
        if child.kind() != "attribute" {
            continue;
        }
        if let Some(name_node) = child_of_type(child, &["attribute_name"]) {
            attrs.insert(
                node_text(data, name_node).to_lowercase(),
                attr_value(data, child),
            );
        }
    }
    attrs
}

fn script_block(data: &[u8], element: Node<'_>) -> Option<ScriptBlock> {
    let start_tag = child_of_type(element, &["start_tag"])?;
    let attrs = tag_attrs(data, start_tag);
    if attrs.contains_key("src") {
        return None;
    }
    let grammar = script_grammar(attrs.get("lang").map(String::as_str).unwrap_or(""))?;
    let raw_text = child_of_type(element, &["raw_text"])?;
    if node_text(data, raw_text).trim().is_empty() {
        return None;
    }
    Some(ScriptBlock {
        grammar,
        is_setup: attrs.contains_key("setup"),
        span: raw_text.range(),
    })
}

pub fn extract_script_blocks(source: &str) -> Result<(Vec<u8>, Vec<ScriptBlock>), String> {
    let (data, tree) = parse(source, "vue")?;
    let mut blocks = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "script_element"
            && let Some(block) = script_block(&data, child)
        {
            blocks.push(block);
        }
    }
    Ok((data, blocks))
}

fn is_ts_function(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
    )
}

fn is_wrapper(kind: &str) -> bool {
    matches!(
        kind,
        "parenthesized_expression" | "as_expression" | "satisfies_expression"
    )
}

fn is_skip_option_key(key: &str) -> bool {
    matches!(
        key,
        "props" | "emits" | "components" | "directives" | "provide" | "inject"
    )
}

fn is_effect_hook(name: &str) -> bool {
    matches!(
        name,
        "watchEffect"
            | "watchPostEffect"
            | "watchSyncEffect"
            | "onBeforeMount"
            | "onMounted"
            | "onBeforeUpdate"
            | "onUpdated"
            | "onBeforeUnmount"
            | "onUnmounted"
            | "onActivated"
            | "onDeactivated"
            | "onErrorCaptured"
            | "onRenderTracked"
            | "onRenderTriggered"
            | "onServerPrefetch"
    )
}

fn named_children_no_comments(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn unwrap_expr(mut node_opt: Option<Node<'_>>) -> Option<Node<'_>> {
    while let Some(node) = node_opt {
        if !is_wrapper(node.kind()) {
            return Some(node);
        }
        let children = named_children_no_comments(node);
        node_opt = children.into_iter().next();
    }
    None
}

fn is_fn_expr(node_opt: Option<Node<'_>>) -> bool {
    let Some(node) = node_opt else {
        return false;
    };
    if node.kind() == "arrow_function" {
        return node.child_by_field_name("body").is_some();
    }
    node.kind() == "function_expression" && child_of_type(node, &["statement_block"]).is_some()
}

fn pair_key(data: &[u8], pair: Node<'_>) -> Option<String> {
    let key = pair.child_by_field_name("key")?;
    match key.kind() {
        "property_identifier" | "identifier" => Some(node_text(data, key).to_string()),
        "string" => Some(
            child_of_type(key, &["string_fragment"])
                .map(|fragment| node_text(data, fragment).to_string())
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

fn object_pairs<'a>(data: &[u8], obj: Node<'a>) -> Vec<(String, Node<'a>)> {
    let mut pairs = Vec::new();
    let mut cursor = obj.walk();
    for child in obj.children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let key = pair_key(data, child);
        let val = unwrap_expr(child.child_by_field_name("value"));
        if let (Some(k), Some(v)) = (key, val) {
            pairs.push((k, v));
        }
    }
    pairs
}

fn pair_fns<'a>(data: &[u8], obj: Node<'a>, keys: Option<&[&str]>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for (key, val) in object_pairs(data, obj) {
        if let Some(allowed) = keys
            && !allowed.contains(&key.as_str())
        {
            continue;
        }
        if is_fn_expr(Some(val)) {
            found.push(val);
        }
    }
    found
}

fn computed_option_fns<'a>(data: &[u8], obj: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for (_key, val) in object_pairs(data, obj) {
        if is_fn_expr(Some(val)) {
            found.push(val);
        } else if val.kind() == "object" {
            found.extend(pair_fns(data, val, Some(&["get", "set"])));
        }
    }
    found
}

fn watch_entry_fns<'a>(data: &[u8], val: Option<Node<'a>>) -> Vec<Node<'a>> {
    let Some(node) = val else {
        return Vec::new();
    };
    if is_fn_expr(Some(node)) {
        return vec![node];
    }
    if node.kind() == "object" {
        return pair_fns(data, node, Some(&["handler"]));
    }
    Vec::new()
}

fn watch_option_fns<'a>(data: &[u8], obj: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for (_key, val) in object_pairs(data, obj) {
        if val.kind() == "array" {
            for item in named_children_no_comments(val) {
                found.extend(watch_entry_fns(data, unwrap_expr(Some(item))));
            }
        } else {
            found.extend(watch_entry_fns(data, Some(val)));
        }
    }
    found
}

fn collect_options<'a>(data: &[u8], obj: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    for (key, val) in object_pairs(data, obj) {
        if is_skip_option_key(&key) {
            continue;
        }
        if val.kind() == "object" {
            match key.as_str() {
                "methods" => found.extend(pair_fns(data, val, None)),
                "computed" => found.extend(computed_option_fns(data, val)),
                "watch" => found.extend(watch_option_fns(data, val)),
                _ => {}
            }
        } else if is_fn_expr(Some(val)) {
            found.push(val);
        }
    }
    found
}

fn callee_name<'a>(data: &'a [u8], callee: Option<Node<'_>>) -> Option<(Option<&'a str>, &'a str)> {
    let target = unwrap_expr(callee)?;
    if target.kind() == "identifier" {
        return Some((None, node_text(data, target)));
    }
    if target.kind() == "member_expression" {
        let obj = unwrap_expr(target.child_by_field_name("object"))?;
        let prop = target.child_by_field_name("property")?;
        if obj.kind() == "identifier" {
            return Some((Some(node_text(data, obj)), node_text(data, prop)));
        }
    }
    None
}

fn fn_or_object<'a, F>(data: &[u8], node_opt: Option<Node<'a>>, on_object: F) -> Vec<Node<'a>>
where
    F: FnOnce(&[u8], Node<'a>) -> Vec<Node<'a>>,
{
    let Some(node) = node_opt else {
        return Vec::new();
    };
    if node.kind() == "object" {
        return on_object(data, node);
    }
    if is_fn_expr(Some(node)) {
        vec![node]
    } else {
        Vec::new()
    }
}

fn computed_object<'a>(data: &[u8], obj: Node<'a>) -> Vec<Node<'a>> {
    pair_fns(data, obj, Some(&["get", "set"]))
}

fn arg_at<'a>(args: &[Node<'a>], index: usize) -> Option<Node<'a>> {
    args.get(index).copied().and_then(|n| unwrap_expr(Some(n)))
}

fn fn_arg<'a>(args: &[Node<'a>], index: usize) -> Vec<Node<'a>> {
    let arg = arg_at(args, index);
    if is_fn_expr(arg) {
        arg.into_iter().collect()
    } else {
        Vec::new()
    }
}

fn composition_call_nodes<'a>(
    data: &[u8],
    resolved: (Option<&str>, &str),
    args: &[Node<'a>],
) -> Vec<Node<'a>> {
    let (receiver, name) = resolved;
    if !matches!(receiver, None | Some("Vue") | Some("vue")) {
        return Vec::new();
    }
    if name == "computed" {
        return fn_or_object(data, arg_at(args, 0), computed_object);
    }
    if name == "watch" {
        return fn_arg(args, 1);
    }
    if is_effect_hook(name) {
        return fn_arg(args, 0);
    }
    Vec::new()
}

fn call_nodes<'a>(data: &[u8], call: Node<'a>) -> Vec<Node<'a>> {
    let Some(resolved) = callee_name(data, call.child_by_field_name("function")) else {
        return Vec::new();
    };
    let Some(args_node) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let args = named_children_no_comments(args_node);
    let is_component_call = matches!(
        resolved,
        (None, "defineComponent")
            | (Some("Vue" | "vue"), "defineComponent")
            | (Some("Vue" | "vue"), "extend")
    );
    if is_component_call {
        return fn_or_object(data, arg_at(&args, 0), collect_options);
    }
    composition_call_nodes(data, resolved, &args)
}

fn export_default_nodes<'a>(data: &[u8], node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    let has_default = node.children(&mut cursor).any(|c| c.kind() == "default");
    if !has_default {
        return Vec::new();
    }
    let value = unwrap_expr(node.child_by_field_name("value"));
    fn_or_object(data, value, collect_options)
}

fn vue_extra_nodes<'a>(data: &[u8], root: Node<'a>) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_ts_function(node.kind()) {
            continue;
        }
        if node.kind() == "export_statement" {
            found.extend(export_default_nodes(data, node));
        } else if node.kind() == "call_expression" {
            found.extend(call_nodes(data, node));
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'a>> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    found
}

/// Extract candidate Vue nodes per `<script>` block and hand deduplicated, byte-sorted nodes to `emit`.
pub fn visit_vue_nodes<F, G>(source: &str, mut pick_base: F, mut emit: G)
where
    F: for<'d, 't> FnMut(&'d [u8], Node<'t>) -> Vec<Node<'t>>,
    G: for<'d, 't> FnMut(&'d [u8], Node<'t>),
{
    let Ok((data, blocks)) = extract_script_blocks(source) else {
        return;
    };
    for block in blocks {
        let Ok(tree) = parse_range(&data, block.grammar.as_str(), block.span) else {
            continue;
        };
        let root = tree.root_node();
        let mut found = pick_base(&data, root);
        found.extend(vue_extra_nodes(&data, root));
        found.sort_by_key(|node| (node.start_byte(), node.end_byte()));
        let mut last_span: Option<(usize, usize)> = None;
        for node in found {
            let span = (node.start_byte(), node.end_byte());
            if last_span == Some(span) {
                continue;
            }
            last_span = Some(span);
            emit(&data, node);
        }
    }
}
