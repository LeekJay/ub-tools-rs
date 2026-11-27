//! Mutation sites and the per-function counts uml-viewer reads.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One replacement inside a function crapper would score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub file: String,
    pub namespace: String,
    pub form_id: String,
    pub name: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub mutant: String,
    pub category: String,
    pub mutation_id: String,
    /// The line its statement starts on; coverage on that line counts for the site.
    #[serde(default)]
    pub anchor_line: usize,
}

impl Site {
    /// Human-readable description of the mutation (`delete X` or `X -> Y`).
    pub fn description(&self) -> String {
        if self.mutant.is_empty() {
            format!("delete {}", self.original)
        } else {
            format!("{} -> {}", self.original, self.mutant)
        }
    }
}

/// Killed, survived, and uncovered totals for one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormResult {
    pub id: String,
    pub namespace: String,
    pub name: String,
    pub private: bool,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub digest: String,
    #[serde(default)]
    pub killed: usize,
    #[serde(default)]
    pub survived: usize,
    #[serde(default)]
    pub uncovered: usize,
    #[serde(default)]
    pub sites: usize,
}

impl FormResult {
    /// Killed fraction among executed mutants, or `None` when none executed.
    pub fn score(&self) -> Option<f64> {
        let executed = self.killed + self.survived;
        if executed == 0 {
            None
        } else {
            Some(self.killed as f64 / executed as f64)
        }
    }
}

/// A form summary loaded from a prior `.metrics/mutate/*.edn` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorForm {
    pub namespace: String,
    pub id: String,
    pub digest: String,
    pub file: String,
}

/// Previous results for one source file, used for differential runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub forms: HashMap<(String, String), PriorForm>,
    pub outcomes: HashMap<String, String>,
}

/// Outcome of mutating one source file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub path: String,
    pub forms: Vec<FormResult>,
    pub written: Vec<String>,
    #[serde(default)]
    pub sites: Vec<Site>,
    #[serde(default)]
    pub statuses: HashMap<String, String>,
    #[serde(default)]
    pub executed_killed: usize,
    #[serde(default)]
    pub executed_survived: usize,
    #[serde(default)]
    pub baseline_failed: bool,
    #[serde(default)]
    pub baseline_message: String,
    #[serde(default)]
    pub skipped: String,
}

/// Structured output from scanning one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFileData {
    pub file: String,
    pub sites: Vec<Site>,
    pub changed: std::collections::HashSet<(String, String)>,
    pub covered: Option<HashMap<String, bool>>,
    /// Unfiltered discovered sites populated only on cache miss so callers can update `SiteCache`.
    pub discovered_sites: Option<Vec<Site>>,
}

/// One mutation site annotated with its optional coverage state for `--scan --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSite {
    #[serde(flatten)]
    pub site: Site,
    pub covered: Option<bool>,
}

/// One mutation site annotated with its run outcome status (`"killed"`, `"survived"`, `"uncovered"`, or `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutateSite {
    #[serde(flatten)]
    pub site: Site,
    pub status: Option<String>,
}

/// A source file excluded from mutation along with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub file: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_form(killed: usize, survived: usize) -> FormResult {
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
            uncovered: 0,
            sites: killed + survived,
        }
    }

    fn sample_site(original: &str, mutant: &str) -> Site {
        Site {
            file: "src/demo.py".to_string(),
            namespace: "demo".to_string(),
            form_id: "defn/place".to_string(),
            name: "place".to_string(),
            line: 2,
            start: 0,
            end: 1,
            original: original.to_string(),
            mutant: mutant.to_string(),
            category: "comparison".to_string(),
            mutation_id: "m".to_string(),
            anchor_line: 0,
        }
    }

    #[test]
    fn score_and_description_follow_counts() {
        assert_eq!(sample_form(1, 1).score(), Some(0.5));
        assert_eq!(sample_form(0, 0).score(), None);
        assert_eq!(sample_form(1, 0).score(), Some(1.0));
        assert_eq!(sample_site(">", ">=").description(), "> -> >=");
        assert_eq!(sample_site("not", "").description(), "delete not");
    }
}
