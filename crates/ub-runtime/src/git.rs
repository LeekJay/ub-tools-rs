use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lock::normalize_repo_path;

/// Error returned when invoking `git` fails or exits non-zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommandError {
    pub code: i32,
    pub message: String,
}

/// Changed lines within a source file returned by git diff inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedLines {
    All,
    Ranges(Vec<(usize, usize)>),
}

impl ChangedLines {
    /// Returns true if an inclusive 1-based line span `[start_line, end_line]` overlaps any change.
    pub fn overlaps(&self, start_line: usize, end_line: usize) -> bool {
        match self {
            Self::All => true,
            Self::Ranges(ranges) => ranges
                .iter()
                .any(|&(s, e)| start_line <= e && end_line >= s),
        }
    }

    /// Returns true if a single 1-based line overlaps any change.
    pub fn contains_line(&self, line: usize) -> bool {
        self.overlaps(line, line)
    }

    /// Expands line ranges into a set of 1-based line numbers, or `None` when the whole file changed.
    pub fn to_line_set(&self) -> Option<HashSet<usize>> {
        match self {
            Self::All => None,
            Self::Ranges(ranges) => {
                let mut set = HashSet::new();
                for &(start, end) in ranges {
                    set.extend(start..=end);
                }
                Some(set)
            }
        }
    }
}

/// Run `git -C <root> <args...>` and return `(exit_code, stdout, stderr)`.
pub fn run_git(root: &Path, args: &[&str]) -> (i32, Vec<u8>, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(1),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(err) => (127, Vec::new(), err.to_string()),
    }
}

fn git_err(code: i32, stderr: &str, fallback: &str) -> GitCommandError {
    let msg = stderr.trim();
    GitCommandError {
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

/// Parse zero-context unified diff output (`git diff -U0 --no-prefix`) into inclusive 1-based line ranges.
pub fn parse_diff_line_ranges(diff_bytes: &[u8]) -> HashMap<String, Vec<(usize, usize)>> {
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

/// Collect git-changed and untracked files under `root`, mapped to their changed 1-based line ranges.
///
/// When `base` is `None`, compares `HEAD` (or unborn working tree) against the working tree;
/// when `Some(ref_name)` is provided, compares `<ref_name>` against the working tree.
pub fn git_changed_lines(
    root: &Path,
    base: Option<&str>,
) -> Result<Vec<(PathBuf, ChangedLines)>, GitCommandError> {
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
    let limit = normalize_repo_path(root);
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
        let path = normalize_repo_path(&top.join(rel_str.as_ref()));
        if path.is_file() && path.starts_with(&limit) {
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
        let path = normalize_repo_path(&top.join(&rel));
        if path.is_file() && path.starts_with(&limit) {
            merge_changed_entry(&mut map, path, ChangedLines::Ranges(ranges));
        }
    }

    Ok(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn changed_lines_helpers_cover_boundaries_and_line_sets() {
        let ranged = ChangedLines::Ranges(vec![(10, 12), (30, 30)]);
        assert!(ranged.overlaps(5, 10));
        assert!(ranged.overlaps(12, 20));
        assert!(ranged.contains_line(11));
        assert!(ranged.contains_line(30));
        assert!(!ranged.overlaps(1, 9));
        assert!(!ranged.overlaps(13, 29));
        assert!(!ranged.contains_line(31));
        assert_eq!(ranged.to_line_set(), Some(HashSet::from([10, 11, 12, 30])));

        let all = ChangedLines::All;
        assert!(all.overlaps(100, 120));
        assert!(all.contains_line(1));
        assert_eq!(all.to_line_set(), None);
    }
}
