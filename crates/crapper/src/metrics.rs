use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::model::Entry;

pub fn metrics_path(root: &Path) -> PathBuf {
    root.join(".metrics").join("crap.edn")
}

fn edn_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn edn_number(value: f64) -> String {
    let normalized = if value == 0.0 { 0.0 } else { value };
    let formatted = format!("{normalized:.4}");
    let mut trimmed = formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string();
    if !trimmed.contains('.') {
        trimmed.push_str(".0");
    }
    trimmed
}

fn edn_value(value: Option<f64>) -> String {
    match value {
        None => "nil".to_string(),
        Some(v) => edn_number(v),
    }
}

/// EDN map `{:entries [...]}` with crap4clj's keys, in crap4clj's order.
pub fn render_edn(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "{:entries []}\n".to_string();
    }
    let rows: Vec<String> = entries
        .iter()
        .map(|entry| {
            format!(
                "  {{:name {}, :namespace {}, :complexity {}, :coverage {}, :crap {}}}",
                edn_string(&entry.name),
                edn_string(&entry.namespace),
                entry.complexity,
                edn_value(entry.coverage),
                edn_value(entry.crap),
            )
        })
        .collect();
    format!("{{:entries [\n{}\n]}}\n", rows.join("\n"))
}

pub fn write_metrics(entries: &[Entry], root: &Path) -> io::Result<PathBuf> {
    let path = metrics_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, render_edn(entries))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(coverage: Option<f64>, crap: Option<f64>) -> Entry {
        Entry {
            name: "layout".to_string(),
            namespace: "uml-viewer.layout".to_string(),
            complexity: 8,
            coverage,
            crap,
            path: "src/layout.clj".to_string(),
            language: "clojure".to_string(),
        }
    }

    #[test]
    fn edn_rendering_and_empty_vector() {
        assert_eq!(render_edn(&[]), "{:entries []}\n");
        let text = render_edn(&[sample_entry(Some(82.0), Some(3.4))]);
        assert!(text.starts_with("{:entries ["));
        assert!(text.contains(":name \"layout\""));
        assert!(text.contains(":namespace \"uml-viewer.layout\""));
        assert!(text.contains(":complexity 8"));
        assert!(text.contains(":coverage 82.0"));
        assert!(text.contains(":crap 3.4"));
        assert!(text.ends_with("}\n"));

        let nil_text = render_edn(&[sample_entry(None, None)]);
        assert!(nil_text.contains(":coverage nil"));
        assert!(nil_text.contains(":crap nil"));
    }
}
