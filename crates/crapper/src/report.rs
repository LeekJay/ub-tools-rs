use std::collections::BTreeSet;

use serde::Serialize;

use crate::model::Entry;

#[derive(Serialize)]
struct JsonSummary {
    functions: usize,
    worst_crap: Option<f64>,
    max_complexity: Option<usize>,
    threshold: Option<f64>,
    threshold_exceeded: bool,
}

#[derive(Serialize)]
struct JsonReport<'a> {
    version: u32,
    tool: &'static str,
    languages: Vec<&'a str>,
    summary: JsonSummary,
    entries: &'a [Entry],
}

pub fn format_json(entries: &[Entry], threshold: Option<f64>) -> String {
    let languages: Vec<&str> = entries
        .iter()
        .map(|e| e.language.as_str())
        .filter(|lang| !lang.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let worst_crap = entries.iter().filter_map(|e| e.crap).max_by(f64::total_cmp);
    let max_complexity = entries.iter().map(|e| e.complexity).max();
    let threshold_exceeded = worst_crap.is_some_and(|w| threshold.is_some_and(|t| w > t));

    let payload = JsonReport {
        version: 1,
        tool: "crapper",
        languages,
        summary: JsonSummary {
            functions: entries.len(),
            worst_crap,
            max_complexity,
            threshold,
            threshold_exceeded,
        },
        entries,
    };
    let mut json = serde_json::to_string_pretty(&payload).expect("serialize CRAP JSON report");
    json.push('\n');
    json
}

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
            start_line: 1,
            end_line: 3,
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

    #[test]
    fn json_report_handles_empty_and_scored_entries() {
        let empty = format_json(&[], None);
        assert!(empty.ends_with('\n'));
        let empty_val: serde_json::Value = serde_json::from_str(&empty).expect("valid empty json");
        assert_eq!(empty_val["version"], 1);
        assert_eq!(empty_val["tool"], "crapper");
        assert_eq!(empty_val["languages"], serde_json::json!([]));
        assert_eq!(empty_val["summary"]["functions"], 0);
        assert_eq!(empty_val["summary"]["worst_crap"], serde_json::Value::Null);
        assert_eq!(
            empty_val["summary"]["max_complexity"],
            serde_json::Value::Null
        );
        assert_eq!(empty_val["summary"]["threshold"], serde_json::Value::Null);
        assert_eq!(empty_val["summary"]["threshold_exceeded"], false);
        assert_eq!(empty_val["entries"], serde_json::json!([]));

        let entries = vec![
            Entry {
                name: "zeta".to_string(),
                namespace: "pkg.z".to_string(),
                complexity: 5,
                coverage: Some(0.0),
                crap: Some(30.0),
                start_line: 10,
                end_line: 25,
                path: "src/z.rs".to_string(),
                language: "rust".to_string(),
            },
            Entry {
                name: "alpha".to_string(),
                namespace: "pkg.a".to_string(),
                complexity: 8,
                coverage: None,
                crap: None,
                start_line: 1,
                end_line: 12,
                path: "src/a.py".to_string(),
                language: "python".to_string(),
            },
            Entry {
                name: "blank".to_string(),
                namespace: "pkg.b".to_string(),
                complexity: 1,
                coverage: Some(100.0),
                crap: Some(1.0),
                start_line: 4,
                end_line: 6,
                path: "src/b.py".to_string(),
                language: String::new(),
            },
        ];

        let below = format_json(&entries, Some(30.0));
        assert!(below.ends_with('\n'));
        let below_val: serde_json::Value = serde_json::from_str(&below).expect("valid below json");
        assert_eq!(
            below_val["languages"],
            serde_json::json!(["python", "rust"])
        );
        assert_eq!(below_val["summary"]["functions"], 3);
        assert_eq!(below_val["summary"]["worst_crap"], 30.0);
        assert_eq!(below_val["summary"]["max_complexity"], 8);
        assert_eq!(below_val["summary"]["threshold"], 30.0);
        assert_eq!(below_val["summary"]["threshold_exceeded"], false);
        assert_eq!(below_val["entries"][0]["name"], "zeta");
        assert_eq!(below_val["entries"][0]["start_line"], 10);
        assert_eq!(below_val["entries"][0]["end_line"], 25);

        let above = format_json(&entries, Some(29.9));
        let above_val: serde_json::Value = serde_json::from_str(&above).expect("valid above json");
        assert_eq!(above_val["summary"]["threshold_exceeded"], true);
    }
}
