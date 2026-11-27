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
    ub_runtime::atomic_write(&path, render_edn(entries).as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static NEXT_DIR_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crapper-metrics-{label}-{}-{nanos}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test temp dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    #[test]
    fn write_metrics_leaves_valid_edn_and_no_stray_tmp_files() {
        let dir = TestDir::new("atomic");
        let initial = vec![sample_entry(Some(50.0), Some(16.0))];
        let updated = vec![sample_entry(Some(82.0), Some(3.4))];

        let first_path = write_metrics(&initial, &dir.0).expect("write initial metrics");
        assert_eq!(first_path, metrics_path(&dir.0));
        let path = write_metrics(&updated, &dir.0).expect("overwrite metrics");
        assert_eq!(path, metrics_path(&dir.0));

        let content = fs::read_to_string(&path).expect("read written metrics");
        assert_eq!(content, render_edn(&updated));
        assert!(content.starts_with("{:entries ["));
        assert!(content.contains(":coverage 82.0"));
        assert!(content.contains(":crap 3.4"));
        assert!(content.ends_with("]}\n"));

        let metrics_dir = path.parent().expect(".metrics dir");
        let names: Vec<String> = fs::read_dir(metrics_dir)
            .expect("list .metrics dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["crap.edn"]);
        assert!(!names.iter().any(|name| name.ends_with(".tmp")));
    }
}
