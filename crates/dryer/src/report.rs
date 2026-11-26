//! Text and EDN reports matching dry4clj and Python dryer's formatting.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::model::{Duplicate, Span};

/// Format duplicate candidates as human-readable text.
pub fn format_text(candidates: &[Duplicate]) -> String {
    if candidates.is_empty() {
        return "No duplicate candidates found.\n".to_string();
    }
    let blocks: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            format!(
                "DUPLICATE score={:.2}\n  {}:{}-{}\n  {}:{}-{}",
                candidate.score,
                candidate.left.file,
                candidate.left.start_line,
                candidate.left.end_line,
                candidate.right.file,
                candidate.right.start_line,
                candidate.right.end_line,
            )
        })
        .collect();
    format!("{}\n", blocks.join("\n\n"))
}

fn edn_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn edn_float(value: f64) -> String {
    let formatted = format!("{value:.12}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed.contains('.') {
        trimmed.to_string()
    } else {
        format!("{trimmed}.0")
    }
}

fn span_edn(span: &Span) -> String {
    format!(
        "{{:file {}, :start-line {}, :end-line {}}}",
        edn_string(&span.file),
        span.start_line,
        span.end_line
    )
}

fn candidate_edn(candidate: &Duplicate) -> String {
    [
        format!(" {{:score {}", edn_float(candidate.score)),
        format!("  :language {}", edn_string(&candidate.language)),
        format!("  :left {}", span_edn(&candidate.left)),
        format!("  :right {}", span_edn(&candidate.right)),
        format!("  :left-nodes {}", candidate.left_nodes),
        format!("  :right-nodes {}}}", candidate.right_nodes),
    ]
    .join("\n")
}

/// Format duplicate candidates as `{:candidates [...]}` EDN.
pub fn render_edn(candidates: &[Duplicate]) -> String {
    if candidates.is_empty() {
        return "{:candidates []}\n".to_string();
    }
    let rows: Vec<String> = candidates.iter().map(candidate_edn).collect();
    format!("{{:candidates [\n{}\n]}}\n", rows.join("\n"))
}

pub fn metrics_path(root: &Path) -> PathBuf {
    root.join(".metrics").join("dry.edn")
}

pub fn write_metrics(candidates: &[Duplicate], root: &Path) -> io::Result<PathBuf> {
    let path = metrics_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, render_edn(candidates))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_edn_reports_match_python() {
        let candidate = Duplicate {
            score: 0.875,
            language: "clojure".into(),
            left: Span {
                file: "a.clj".into(),
                start_line: 10,
                end_line: 14,
            },
            right: Span {
                file: "b.clj".into(),
                start_line: 20,
                end_line: 24,
            },
            left_nodes: 30,
            right_nodes: 31,
        };
        assert_eq!(
            format_text(std::slice::from_ref(&candidate)),
            "DUPLICATE score=0.88\n  a.clj:10-14\n  b.clj:20-24\n"
        );
        assert_eq!(format_text(&[]), "No duplicate candidates found.\n");
        assert_eq!(
            render_edn(&[candidate]),
            "{:candidates [\n {:score 0.875\n  :language \"clojure\"\n  :left {:file \"a.clj\", :start-line 10, :end-line 14}\n  :right {:file \"b.clj\", :start-line 20, :end-line 24}\n  :left-nodes 30\n  :right-nodes 31}\n]}\n"
        );
        assert_eq!(render_edn(&[]), "{:candidates []}\n");
    }

    #[test]
    fn edn_float_handles_whole_and_long_decimals() {
        assert_eq!(edn_float(1.0), "1.0");
        assert_eq!(edn_float(0.0), "0.0");
        assert_eq!(edn_float(0.8909090909090909), "0.890909090909");
    }
}
