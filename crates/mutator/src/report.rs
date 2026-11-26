//! Text reports for a mutation run and for `--scan`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::model::{FormResult, Site};

fn format_score(form: &FormResult) -> String {
    match form.score() {
        None => "   n/a".to_string(),
        Some(score) => format!("{:5.1}%", score * 100.0),
    }
}

fn pad_left(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Format per-function killed/survived/uncovered/score table.
pub fn format_results(forms: &[FormResult]) -> String {
    if forms.is_empty() {
        return "No functions to mutate.\n".to_string();
    }
    let max_name = forms
        .iter()
        .map(|f| f.name.chars().count())
        .max()
        .unwrap_or(0);
    let max_ns = forms
        .iter()
        .map(|f| f.namespace.chars().count())
        .max()
        .unwrap_or(0);
    let name_width = max_name.clamp(8, 48);
    let namespace_width = max_ns.clamp(9, 72);

    let header = format!(
        "{} {} {:>7} {:>9} {:>10} {:>6} {:>7}",
        pad_left("Function", name_width),
        pad_left("Namespace", namespace_width),
        "Killed",
        "Survived",
        "Uncovered",
        "Sites",
        "Score"
    );

    let mut ordered: Vec<&FormResult> = forms.iter().collect();
    ordered.sort_by(|a, b| {
        let sa = a.score();
        let sb = b.score();
        sa.is_some()
            .cmp(&sb.is_some())
            .then_with(|| {
                sa.unwrap_or(0.0)
                    .partial_cmp(&sb.unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| b.survived.cmp(&a.survived))
            .then_with(|| a.name.cmp(&b.name))
    });

    let divider = "-".repeat(header.chars().count());
    let mut lines = Vec::with_capacity(ordered.len() + 5);
    lines.push(header);
    lines.push(divider.clone());

    let mut killed = 0usize;
    let mut survived = 0usize;
    let mut uncovered = 0usize;
    let mut total_sites = 0usize;

    for form in ordered {
        lines.push(format!(
            "{} {} {:7} {:9} {:10} {:6} {}",
            pad_left(&form.name, name_width),
            pad_left(&form.namespace, namespace_width),
            form.killed,
            form.survived,
            form.uncovered,
            form.sites,
            format_score(form),
        ));
        killed += form.killed;
        survived += form.survived;
        uncovered += form.uncovered;
        total_sites += form.sites;
    }

    let executed = killed + survived;
    let total_score = if executed == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", (killed as f64 / executed as f64) * 100.0)
    };

    lines.push(divider);
    lines.push(format!(
        "{} {} {:7} {:9} {:10} {:6} {:>7}",
        pad_left("Total", name_width),
        " ".repeat(namespace_width),
        killed,
        survived,
        uncovered,
        total_sites,
        total_score,
    ));
    lines.push(String::new());
    lines.join("\n")
}

/// Format per-site status lines (`KILLED`, `SURVIVED`, `UNCOVERED`).
pub fn format_site_log(sites: &[Site], status: &HashMap<String, String>) -> String {
    let mut lines = Vec::new();
    for site in sites {
        let Some(label) = status.get(&site.mutation_id) else {
            continue;
        };
        lines.push(format!(
            "{:<9} {}:{} {}",
            label.to_uppercase(),
            site.file,
            site.line,
            site.description()
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Format `--scan` site listing for one file.
pub fn format_scan(
    path: &str,
    sites: &[Site],
    changed: &HashSet<(String, String)>,
    covered: Option<&HashMap<String, bool>>,
) -> String {
    let mut lines = Vec::with_capacity(sites.len() + 3);
    lines.push(format!("Scan: {} mutation sites in {path}", sites.len()));
    for site in sites {
        let mark = if changed.contains(&(site.namespace.clone(), site.form_id.clone())) {
            "*"
        } else {
            " "
        };
        let coverage = match covered {
            Some(map) if !map.get(&site.mutation_id).copied().unwrap_or(true) => " uncovered",
            _ => "",
        };
        lines.push(format!(
            "{mark} {path}:{} {}{coverage}  [{}]",
            site.line,
            site.description(),
            site.form_id
        ));
    }
    if !changed.is_empty() {
        lines.push("* marks a function whose text changed since the last snapshot.".to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(killed: usize, survived: usize, uncovered: usize, sites: usize) -> FormResult {
        FormResult {
            id: "defn/place".to_string(),
            namespace: "demo".to_string(),
            name: "place".to_string(),
            private: false,
            file: "a.py".to_string(),
            line: 2,
            end_line: 4,
            digest: "d".to_string(),
            killed,
            survived,
            uncovered,
            sites,
        }
    }

    fn site() -> Site {
        Site {
            file: "src/demo.py".to_string(),
            namespace: "demo".to_string(),
            form_id: "defn/place".to_string(),
            name: "place".to_string(),
            line: 2,
            start: 0,
            end: 1,
            original: ">".to_string(),
            mutant: ">=".to_string(),
            category: "comparison".to_string(),
            mutation_id: "m".to_string(),
            anchor_line: 0,
        }
    }

    #[test]
    fn format_results_and_scan_match_python() {
        assert_eq!(format_results(&[]), "No functions to mutate.\n");
        let text = format_results(&[form(1, 1, 0, 2)]);
        assert!(
            text.lines()
                .any(|l| l.contains("place") && l.contains("50.0%"))
        );
        let total = text.lines().find(|l| l.starts_with("Total")).unwrap();
        assert_eq!(
            total.split_whitespace().collect::<Vec<_>>(),
            vec!["Total", "1", "1", "0", "2", "50.0%"]
        );

        let perfect = format_results(&[form(1, 0, 0, 1)]);
        let perfect_total = perfect.lines().find(|l| l.starts_with("Total")).unwrap();
        assert_eq!(perfect_total.split_whitespace().last(), Some("100.0%"));

        let idle = format_results(&[form(0, 0, 2, 2)]);
        let idle_total = idle.lines().find(|l| l.starts_with("Total")).unwrap();
        assert_eq!(idle_total.split_whitespace().last(), Some("n/a"));

        let empty_changed = HashSet::new();
        let covered_true = HashMap::from([("m".to_string(), true)]);
        let covered_empty = HashMap::new();
        let covered_false = HashMap::from([("m".to_string(), false)]);
        assert!(
            !format_scan(
                "src/demo.py",
                &[site()],
                &empty_changed,
                Some(&covered_true)
            )
            .contains("uncovered")
        );
        assert!(
            !format_scan(
                "src/demo.py",
                &[site()],
                &empty_changed,
                Some(&covered_empty)
            )
            .contains("uncovered")
        );
        assert!(
            format_scan(
                "src/demo.py",
                &[site()],
                &empty_changed,
                Some(&covered_false)
            )
            .contains("uncovered")
        );
    }
}
