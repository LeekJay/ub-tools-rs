//! Jaccard similarity over structural fingerprints of normalized syntax trees.

use std::collections::HashSet;

/// A node in a normalized syntax tree, matching dry4clj's keyword/string/vector representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormNode {
    Keyword(String),
    Text(String),
    List(Vec<NormNode>),
}

impl Drop for NormNode {
    fn drop(&mut self) {
        // Iterative drop so 1,200+ deep expressions never overflow the call stack.
        let NormNode::List(children) = self else {
            return;
        };
        let mut stack = std::mem::take(children);
        while let Some(mut node) = stack.pop() {
            if let NormNode::List(inner) = &mut node {
                stack.append(inner);
            }
        }
    }
}

fn pr_atom(kind: &NormNode) -> String {
    match kind {
        NormNode::Keyword(name) => format!(":{name}"),
        NormNode::Text(value) => {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
        NormNode::List(_) => unreachable!("pr_atom called on list"),
    }
}

enum PrintStep<'a> {
    Enter(&'a NormNode),
    AssembleList(usize),
}

fn collect_prints_and_fingerprints(
    root: &NormNode,
    collect_set: bool,
) -> (String, HashSet<String>) {
    let mut set = HashSet::new();
    let mut value_stack: Vec<String> = Vec::new();
    let mut work = vec![PrintStep::Enter(root)];

    while let Some(step) = work.pop() {
        match step {
            PrintStep::Enter(node) => match node {
                NormNode::List(children) => {
                    work.push(PrintStep::AssembleList(children.len()));
                    for child in children.iter().rev() {
                        work.push(PrintStep::Enter(child));
                    }
                }
                atom => {
                    let s = pr_atom(atom);
                    if collect_set {
                        set.insert(s.clone());
                    }
                    value_stack.push(s);
                }
            },
            PrintStep::AssembleList(count) => {
                let start = value_stack.len() - count;
                let mut total_len = 2 + count.saturating_sub(1);
                for piece in &value_stack[start..] {
                    total_len += piece.len();
                }
                let mut out = String::with_capacity(total_len);
                out.push('[');
                for (idx, piece) in value_stack.drain(start..).enumerate() {
                    if idx > 0 {
                        out.push(' ');
                    }
                    out.push_str(&piece);
                }
                out.push(']');
                if collect_set {
                    set.insert(out.clone());
                }
                value_stack.push(out);
            }
        }
    }

    let root_str = value_stack.pop().unwrap_or_default();
    (root_str, set)
}

/// Print a normalized node the way Clojure `pr-str` prints it.
pub fn pr(node: &NormNode) -> String {
    collect_prints_and_fingerprints(node, false).0
}

/// Count one plus every nested node; atoms count as one.
pub fn node_count(node: &NormNode) -> usize {
    let mut total = 0;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        total += 1;
        if let NormNode::List(children) = current {
            stack.extend(children.iter());
        }
    }
    total
}

/// Return the printed string of every subtree and atom in `node`.
pub fn fingerprints(node: &NormNode) -> HashSet<String> {
    collect_prints_and_fingerprints(node, true).1
}

/// Jaccard similarity: `|left ∩ right| / |left ∪ right|`, or `0.0` when both are empty.
pub fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 0.0;
    }
    let (smaller, larger) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let shared = smaller.iter().filter(|item| larger.contains(*item)).count();
    let union = left.len() + right.len() - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_counts_and_fingerprints_match_dry4clj() {
        let tree = NormNode::List(vec![
            NormNode::Keyword("list".into()),
            NormNode::Keyword("symbol".into()),
            NormNode::Text("a\\b\"c".into()),
        ]);
        assert_eq!(pr(&tree), "[:list :symbol \"a\\\\b\\\"c\"]");
        assert_eq!(node_count(&NormNode::Keyword("symbol".into())), 1);
        assert_eq!(node_count(&tree), 4);
        let fp = fingerprints(&tree);
        assert_eq!(
            fp,
            HashSet::from([
                ":list".to_string(),
                ":symbol".to_string(),
                "\"a\\\\b\\\"c\"".to_string(),
                "[:list :symbol \"a\\\\b\\\"c\"]".to_string(),
            ])
        );
    }

    #[test]
    fn handles_deep_trees_without_stack_overflow() {
        let mut node = NormNode::Keyword("symbol".into());
        for _ in 0..2000 {
            node = NormNode::List(vec![NormNode::Keyword("list".into()), node]);
        }
        assert_eq!(node_count(&node), 4001);
        assert!(!fingerprints(&node).is_empty());
    }
}
