use std::collections::BTreeSet;

use crate::model::Entry;

fn format_coverage(coverage: Option<f64>) -> String {
    match coverage {
        None => "  N/A ".to_string(),
        Some(v) => format!("{v:5.1}%"),
    }
}

fn format_crap(score: Option<f64>) -> String {
    match score {
        None => "     N/A".to_string(),
        Some(v) => format!("{v:8.1}"),
    }
}

pub fn format_report(entries: &[Entry]) -> String {
    let mut name_width = 30usize;
    let mut namespace_width = 35usize;
    if !entries.is_empty() {
        let max_name = entries
            .iter()
            .map(|e| e.name.chars().count())
            .max()
            .unwrap_or(0);
        let max_ns = entries
            .iter()
            .map(|e| e.namespace.chars().count())
            .max()
            .unwrap_or(0);
        name_width = name_width.max(max_name.min(48));
        namespace_width = namespace_width.max(max_ns.min(72));
    }

    let header = format!(
        "{:<name_width$} {:<namespace_width$} {:>4} {:>7} {:>8}",
        "Function", "Namespace", "CC", "Cov%", "CRAP"
    );
    let separator = "-".repeat(header.chars().count());
    let langs: BTreeSet<&str> = entries.iter().map(|e| e.language.as_str()).collect();
    let languages: String = langs.into_iter().collect::<Vec<_>>().join(", ");

    let mut lines = vec!["CRAP Report".to_string(), "===========".to_string()];
    if !languages.is_empty() {
        lines.push(format!("Languages: {languages}"));
        lines.push(String::new());
    }
    lines.push(header);
    lines.push(separator);
    for entry in entries {
        lines.push(format!(
            "{:<name_width$} {:<namespace_width$} {:4} {} {}",
            entry.name,
            entry.namespace,
            entry.complexity,
            format_coverage(entry.coverage),
            format_crap(entry.crap),
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_shows_na_and_aligns_unicode() {
        let entry = Entry {
            name: "greet".to_string(),
            namespace: "café".to_string(),
            complexity: 2,
            coverage: None,
            crap: None,
            path: "src/café.py".to_string(),
            language: "python".to_string(),
        };
        let text = format_report(&[entry]);
        assert!(text.contains("CRAP Report"));
        assert!(text.contains("N/A"));
        assert!(text.contains("Languages: python"));
        // Verify line alignment uses char count (35 chars for namespace), matching Python.
        let row = text.lines().find(|l| l.starts_with("greet")).unwrap();
        assert_eq!(row.chars().count(), 30 + 1 + 35 + 1 + 4 + 1 + 6 + 1 + 8);
    }
}
