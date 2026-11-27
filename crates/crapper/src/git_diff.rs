use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::discover::{is_test_file, language_of};
use crate::model::Entry;
use crate::path_util::resolve_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChangedLines {
    All,
    Ranges(Vec<(usize, usize)>),
}

pub(crate) fn run_git(root: &Path, args: &[&str]) -> (i32, Vec<u8>, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(1),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(err) => (127, Vec::new(), err.to_string()),
    }
}

fn git_err(code: i32, stderr: &str, fallback: &str) -> GitError {
    let msg = stderr.trim();
    GitError {
        code,
        message: if msg.is_empty() {
            fallback.to_string()
        } else {
            msg.to_string()
        },
    }
}

fn parse_hunk_new_range(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ ")?;
    let plus_idx = rest.find('+')?;
    let after_plus = &rest[plus_idx + 1..];
    let token_end = after_plus
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(after_plus.len());
    let token = &after_plus[..token_end];
    let (start_str, count_str) = match token.split_once(',') {
        Some((s, c)) => (s, Some(c)),
        None => (token, None),
    };
    let start: usize = start_str.parse().ok()?;
    let count: usize = match count_str {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    if count == 0 {
        let line_num = start.max(1);
        Some((line_num, line_num))
    } else {
        Some((start, start.saturating_add(count - 1)))
    }
}

pub(crate) fn parse_diff_line_ranges(diff_bytes: &[u8]) -> HashMap<String, Vec<(usize, usize)>> {
    let text = String::from_utf8_lossy(diff_bytes);
    let mut ranges_by_file: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut current_file: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with("diff --git ") {
            current_file = None;
            continue;
        }
        if let Some(raw_path) = line.strip_prefix("+++ ") {
            let cleaned = raw_path.strip_suffix('\t').unwrap_or(raw_path);
            current_file = if cleaned == "/dev/null" {
                None
            } else {
                Some(cleaned.to_string())
            };
            continue;
        }
        if line.starts_with("@@ ")
            && let Some(file) = current_file.as_ref()
            && let Some(range) = parse_hunk_new_range(line)
        {
            ranges_by_file.entry(file.clone()).or_default().push(range);
        }
    }
    ranges_by_file
}

fn is_candidate_source(path: &Path, limit: &Path) -> bool {
    path.is_file() && path.starts_with(limit) && language_of(path).is_some() && !is_test_file(path)
}

fn merge_changed_entry(
    map: &mut BTreeMap<PathBuf, ChangedLines>,
    path: PathBuf,
    addition: ChangedLines,
) {
    match (map.get_mut(&path), addition) {
        (Some(existing), ChangedLines::All) => *existing = ChangedLines::All,
        (Some(ChangedLines::All), ChangedLines::Ranges(_)) => {}
        (Some(ChangedLines::Ranges(existing)), ChangedLines::Ranges(mut more)) => {
            existing.append(&mut more);
        }
        (None, add) => {
            map.insert(path, add);
        }
    }
}

pub(crate) fn changed_scope(
    root: &Path,
    base: Option<&str>,
) -> Result<Vec<(PathBuf, ChangedLines)>, GitError> {
    let (top_code, top_out, top_err) = run_git(root, &["rev-parse", "--show-toplevel"]);
    if top_code != 0 {
        return Err(git_err(top_code, &top_err, "git status failed"));
    }
    let (st_code, st_out, st_err) = run_git(
        root,
        &[
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--",
            ".",
        ],
    );
    if st_code != 0 {
        return Err(git_err(st_code, &st_err, "git status failed"));
    }

    let top = PathBuf::from(String::from_utf8_lossy(&top_out).trim());
    let limit = resolve_path(root);
    let mut map: BTreeMap<PathBuf, ChangedLines> = BTreeMap::new();

    let (head_code, _, _) = run_git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]);
    let unborn_head = base.is_none() && head_code != 0;

    for entry in st_out.split(|&b| b == 0) {
        if entry.len() < 4 {
            continue;
        }
        let is_untracked = entry.starts_with(b"?? ");
        if !is_untracked && !unborn_head {
            continue;
        }
        let rel_str = String::from_utf8_lossy(&entry[3..]);
        let path = resolve_path(&top.join(rel_str.as_ref()));
        if is_candidate_source(&path, &limit) {
            merge_changed_entry(&mut map, path, ChangedLines::All);
        }
    }

    if unborn_head {
        return Ok(map.into_iter().collect());
    }

    let diff_target = base.unwrap_or("HEAD");
    let (diff_code, diff_out, diff_err) = run_git(
        root,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "-U0",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            "--no-prefix",
            diff_target,
            "--",
            ".",
        ],
    );
    if diff_code != 0 {
        return Err(git_err(diff_code, &diff_err, "git diff failed"));
    }

    for (rel, ranges) in parse_diff_line_ranges(&diff_out) {
        if ranges.is_empty() {
            continue;
        }
        let path = resolve_path(&top.join(&rel));
        if is_candidate_source(&path, &limit) {
            merge_changed_entry(&mut map, path, ChangedLines::Ranges(ranges));
        }
    }

    Ok(map.into_iter().collect())
}

pub(crate) fn entry_overlaps_changes(
    entry: &Entry,
    changed: &HashMap<String, ChangedLines>,
) -> bool {
    match changed.get(&entry.path) {
        Some(ChangedLines::All) => true,
        Some(ChangedLines::Ranges(ranges)) => ranges
            .iter()
            .any(|&(s, e)| entry.start_line <= e && entry.end_line >= s),
        None => false,
    }
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
    fn parses_multi_single_deletion_devnull_and_space_headers() {
        let diff = concat!(
            "diff --git src/my file.ts src/my file.ts\n",
            "index 1111111..2222222 100644\n",
            "--- src/my file.ts\t\n",
            "+++ src/my file.ts\t\n",
            "@@ -10,2 +10,3 @@ export function alpha() {\n",
            "@@ -30 +32 @@ export function beta() {\n",
            "@@ -45,2 +46,0 @@ export function deletedTail() {\n",
            "@@ -1,1 +0,0 @@ top removal\n",
            "diff --git src/removed.ts /dev/null\n",
            "deleted file mode 100644\n",
            "--- src/removed.ts\n",
            "+++ /dev/null\n",
            "@@ -1,5 +0,0 @@\n",
            "diff --git src/other.py src/other.py\n",
            "--- src/other.py\r\n",
            "+++ src/other.py\r\n",
            "@@ -5 +5,2 @@ def second():\r\n",
        );

        let parsed = parse_diff_line_ranges(diff.as_bytes());
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get("src/my file.ts"),
            Some(&vec![(10, 12), (32, 32), (46, 46), (1, 1)])
        );
        assert_eq!(parsed.get("src/other.py"), Some(&vec![(5, 6)]));
        assert!(!parsed.contains_key("/dev/null"));
    }

    #[test]
    fn entry_overlap_checks_boundaries_and_whole_file_markers() {
        let mut map = HashMap::new();
        map.insert(
            "src/app.py".to_string(),
            ChangedLines::Ranges(vec![(10, 12), (30, 30)]),
        );
        map.insert("src/new.py".to_string(), ChangedLines::All);

        // Exact edge overlaps
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

        // Adjacent untouched functions before and after
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

        // New file always matches, missing file never matches
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
