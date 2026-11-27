use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::discover::{is_test_file, language_of};
use crate::model::Entry;

pub(crate) use ub_runtime::{ChangedLines, run_git};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub code: i32,
    pub message: String,
}

impl From<ub_runtime::GitCommandError> for GitError {
    fn from(err: ub_runtime::GitCommandError) -> Self {
        Self {
            code: err.code,
            message: err.message,
        }
    }
}

pub(crate) fn changed_scope(
    root: &Path,
    base: Option<&str>,
) -> Result<Vec<(PathBuf, ChangedLines)>, GitError> {
    let entries = ub_runtime::git_changed_lines(root, base)?;
    Ok(entries
        .into_iter()
        .filter(|(path, _)| language_of(path).is_some() && !is_test_file(path))
        .collect())
}

pub(crate) fn entry_overlaps_changes(
    entry: &Entry,
    changed: &HashMap<String, ChangedLines>,
) -> bool {
    changed
        .get(&entry.path)
        .is_some_and(|lines| lines.overlaps(entry.start_line, entry.end_line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(path: &str, start_line: usize, end_line: usize) -> Entry {
        Entry {
            name: "fn_a".to_string(),
            namespace: "demo".to_string(),
            complexity: 2,
            coverage: None,
            crap: None,
            start_line,
            end_line,
            path: path.to_string(),
            language: "python".to_string(),
        }
    }

    #[test]
    fn entry_overlap_checks_boundaries_and_whole_file_markers() {
        let mut map = HashMap::new();
        map.insert(
            "src/app.py".to_string(),
            ChangedLines::Ranges(vec![(10, 12), (30, 30)]),
        );
        map.insert("src/new.py".to_string(), ChangedLines::All);

        assert!(entry_overlaps_changes(
            &sample_entry("src/app.py", 5, 10),
            &map
        ));
        assert!(entry_overlaps_changes(
            &sample_entry("src/app.py", 12, 20),
            &map
        ));
        assert!(entry_overlaps_changes(
            &sample_entry("src/app.py", 25, 35),
            &map
        ));
        assert!(!entry_overlaps_changes(
            &sample_entry("src/app.py", 1, 9),
            &map
        ));
        assert!(!entry_overlaps_changes(
            &sample_entry("src/app.py", 13, 29),
            &map
        ));
        assert!(!entry_overlaps_changes(
            &sample_entry("src/app.py", 31, 50),
            &map
        ));
        assert!(entry_overlaps_changes(
            &sample_entry("src/new.py", 100, 120),
            &map
        ));
        assert!(!entry_overlaps_changes(
            &sample_entry("src/unlisted.py", 1, 100),
            &map
        ));
    }
}
