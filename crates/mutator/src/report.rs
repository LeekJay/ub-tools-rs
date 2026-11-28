//! Text and JSON reports for a mutation run and for `--scan`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::model::{FormResult, MutateSite, ScanFileData, ScanSite, Site, SkippedFile};

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

/// Inputs for formatting a mutation run as structured JSON.
#[derive(Debug, Clone, Copy, Default)]
pub struct MutateReport<'a> {
    pub forms: &'a [FormResult],
    pub sites: &'a [MutateSite],
    pub written: &'a [String],
    pub skipped: &'a [SkippedFile],
    pub executed_killed: usize,
    pub executed_survived: usize,
    pub baseline_failed: bool,
    pub scoped: bool,
}

#[derive(Serialize)]
struct ScanSummaryJson {
    files: usize,
    sites: usize,
    covered: usize,
    uncovered: usize,
    changed_forms: usize,
}

#[derive(Serialize)]
struct ScanFileJson<'a> {
    file: &'a str,
    changed_forms: Vec<[String; 2]>,
    sites: Vec<ScanSite>,
}

#[derive(Serialize)]
struct ScanJson<'a> {
    version: u32,
    tool: &'static str,
    mode: &'static str,
    summary: ScanSummaryJson,
    files: Vec<ScanFileJson<'a>>,
}

/// Format `--scan` results as a single JSON object terminated with `\n`.
///
/// When coverage is ignored (`--no-coverage`), each site's `covered` field
/// is `null` and `summary.covered` / `summary.uncovered` stay `0`.
pub fn format_scan_json(files: &[ScanFileData]) -> String {
    let mut total_sites = 0usize;
    let mut total_covered = 0usize;
    let mut total_uncovered = 0usize;
    let mut total_changed_forms = 0usize;

    let json_files: Vec<ScanFileJson<'_>> = files
        .iter()
        .map(|entry| {
            let mut changed_forms: Vec<[String; 2]> = entry
                .changed
                .iter()
                .map(|(ns, form_id)| [ns.clone(), form_id.clone()])
                .collect();
            changed_forms.sort();
            total_changed_forms += changed_forms.len();

            let sites: Vec<ScanSite> = entry
                .sites
                .iter()
                .map(|site| {
                    let covered = entry
                        .covered
                        .as_ref()
                        .and_then(|map| map.get(&site.mutation_id).copied());
                    match covered {
                        Some(true) => total_covered += 1,
                        Some(false) => total_uncovered += 1,
                        None => {}
                    }
                    ScanSite {
                        site: site.clone(),
                        covered,
                    }
                })
                .collect();
            total_sites += sites.len();

            ScanFileJson {
                file: &entry.file,
                changed_forms,
                sites,
            }
        })
        .collect();

    let payload = ScanJson {
        version: 1,
        tool: "mutator",
        mode: "scan",
        summary: ScanSummaryJson {
            files: files.len(),
            sites: total_sites,
            covered: total_covered,
            uncovered: total_uncovered,
            changed_forms: total_changed_forms,
        },
        files: json_files,
    };
    let mut out = serde_json::to_string(&payload).expect("scan JSON serialization");
    out.push('\n');
    out
}

#[derive(Serialize)]
struct MutateSummaryJson {
    killed: usize,
    survived: usize,
    executed_killed: usize,
    executed_survived: usize,
    uncovered: usize,
    sites: usize,
    score: Option<f64>,
    baseline_failed: bool,
}

#[derive(Serialize)]
struct MutateFormJson<'a> {
    #[serde(flatten)]
    form: &'a FormResult,
    score: Option<f64>,
}

#[derive(Serialize)]
struct MutateJson<'a> {
    version: u32,
    tool: &'static str,
    mode: &'static str,
    summary: MutateSummaryJson,
    forms: Vec<MutateFormJson<'a>>,
    sites: &'a [MutateSite],
    written: &'a [String],
    skipped: &'a [SkippedFile],
}

/// Format mutation run results as a single JSON object terminated with `\n`.
pub fn format_mutate_json(report: &MutateReport<'_>) -> String {
    let killed: usize = report.forms.iter().map(|f| f.killed).sum();
    let survived: usize = report.forms.iter().map(|f| f.survived).sum();
    let uncovered: usize = report.forms.iter().map(|f| f.uncovered).sum();
    let total_sites: usize = if report.scoped || report.forms.is_empty() {
        report.sites.len()
    } else {
        report.forms.iter().map(|f| f.sites).sum()
    };
    let executed = killed + survived;
    let score = if executed == 0 {
        None
    } else {
        Some(killed as f64 / executed as f64)
    };

    let forms: Vec<MutateFormJson<'_>> = report
        .forms
        .iter()
        .map(|form| MutateFormJson {
            form,
            score: form.score(),
        })
        .collect();

    let payload = MutateJson {
        version: 1,
        tool: "mutator",
        mode: "mutate",
        summary: MutateSummaryJson {
            killed,
            survived,
            executed_killed: report.executed_killed,
            executed_survived: report.executed_survived,
            uncovered,
            sites: total_sites,
            score,
            baseline_failed: report.baseline_failed,
        },
        forms,
        sites: report.sites,
        written: report.written,
        skipped: report.skipped,
    };
    let mut out = serde_json::to_string(&payload).expect("mutate JSON serialization");
    out.push('\n');
    out
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

    #[test]
    fn format_scan_json_covers_evaluated_and_ignored_coverage_and_empty() {
        let empty = format_scan_json(&[]);
        assert!(empty.ends_with('\n'));
        let empty_val: serde_json::Value = serde_json::from_str(&empty).unwrap();
        assert_eq!(empty_val["version"], 1);
        assert_eq!(empty_val["tool"], "mutator");
        assert_eq!(empty_val["mode"], "scan");
        assert_eq!(
            empty_val["summary"],
            serde_json::json!({
                "files": 0,
                "sites": 0,
                "covered": 0,
                "uncovered": 0,
                "changed_forms": 0
            })
        );
        assert_eq!(empty_val["files"], serde_json::json!([]));

        let s1 = site();
        let mut s2 = site();
        s2.line = 3;
        s2.mutation_id = "m2".to_string();

        let with_cov = ScanFileData {
            file: "src/demo.py".to_string(),
            sites: vec![s1.clone(), s2],
            changed: HashSet::from([
                ("demo".to_string(), "defn/z".to_string()),
                ("demo".to_string(), "defn/a".to_string()),
            ]),
            covered: Some(HashMap::from([
                ("m".to_string(), true),
                ("m2".to_string(), false),
            ])),
            discovered_sites: None,
        };
        let without_cov = ScanFileData {
            file: "src/other.py".to_string(),
            sites: vec![s1],
            changed: HashSet::new(),
            covered: None,
            discovered_sites: None,
        };

        let out = format_scan_json(&[with_cov, without_cov]);
        assert!(out.ends_with('\n'));
        let val: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(val["summary"]["files"], 2);
        assert_eq!(val["summary"]["sites"], 3);
        assert_eq!(val["summary"]["covered"], 1);
        assert_eq!(val["summary"]["uncovered"], 1);
        assert_eq!(val["summary"]["changed_forms"], 2);

        assert_eq!(
            val["files"][0]["changed_forms"],
            serde_json::json!([["demo", "defn/a"], ["demo", "defn/z"]])
        );
        assert_eq!(val["files"][0]["sites"][0]["file"], "src/demo.py");
        assert_eq!(val["files"][0]["sites"][0]["original"], ">");
        assert_eq!(val["files"][0]["sites"][0]["anchor_line"], 0);
        assert_eq!(val["files"][0]["sites"][0]["covered"], true);
        assert_eq!(val["files"][0]["sites"][1]["covered"], false);
        assert_eq!(
            val["files"][1]["sites"][0]["covered"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn format_mutate_json_handles_outcomes_empty_and_baseline_failure() {
        let empty = format_mutate_json(&MutateReport::default());
        assert!(empty.ends_with('\n'));
        let empty_val: serde_json::Value = serde_json::from_str(&empty).unwrap();
        assert_eq!(empty_val["version"], 1);
        assert_eq!(empty_val["tool"], "mutator");
        assert_eq!(empty_val["mode"], "mutate");
        assert_eq!(
            empty_val["summary"],
            serde_json::json!({
                "killed": 0,
                "survived": 0,
                "executed_killed": 0,
                "executed_survived": 0,
                "uncovered": 0,
                "sites": 0,
                "score": null,
                "baseline_failed": false
            })
        );
        assert_eq!(empty_val["forms"], serde_json::json!([]));
        assert_eq!(empty_val["sites"], serde_json::json!([]));
        assert_eq!(empty_val["written"], serde_json::json!([]));
        assert_eq!(empty_val["skipped"], serde_json::json!([]));

        let forms = [form(1, 1, 1, 3)];
        let sites = [
            MutateSite {
                site: site(),
                status: Some("killed".to_string()),
            },
            MutateSite {
                site: site(),
                status: Some("survived".to_string()),
            },
            MutateSite {
                site: site(),
                status: Some("uncovered".to_string()),
            },
        ];
        let written = [".metrics/mutate/demo.edn".to_string()];
        let skipped = [SkippedFile {
            file: "bin.dat".to_string(),
            reason: "file is not UTF-8".to_string(),
        }];
        let out = format_mutate_json(&MutateReport {
            forms: &forms,
            sites: &sites,
            written: &written,
            skipped: &skipped,
            executed_killed: 1,
            executed_survived: 0,
            baseline_failed: true,
            scoped: false,
        });
        assert!(out.ends_with('\n'));
        let val: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(val["summary"]["killed"], 1);
        assert_eq!(val["summary"]["survived"], 1);
        assert_eq!(val["summary"]["executed_killed"], 1);
        assert_eq!(val["summary"]["executed_survived"], 0);
        assert_eq!(val["summary"]["uncovered"], 1);
        assert_eq!(val["summary"]["sites"], 3);
        assert_eq!(val["summary"]["score"], 0.5);
        assert_eq!(val["summary"]["baseline_failed"], true);
        assert_eq!(val["forms"][0]["score"], 0.5);
        assert_eq!(val["sites"][0]["status"], "killed");
        assert_eq!(val["sites"][1]["status"], "survived");
        assert_eq!(
            val["written"],
            serde_json::json!([".metrics/mutate/demo.edn"])
        );
        assert_eq!(
            val["skipped"],
            serde_json::json!([{"file": "bin.dat", "reason": "file is not UTF-8"}])
        );
    }
}
