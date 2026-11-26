//! Read source files and pair forms that meet the similarity threshold.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::discover::{language_of, resolve_path, to_posix};
use super::extract::entries_in_source;
use super::model::{Duplicate, Entry, Span};
use super::shape::jaccard;

fn display_path_resolved(path: &Path, root_resolved: &Path) -> String {
    let resolved = resolve_path(path);
    match resolved.strip_prefix(root_resolved) {
        Ok(rel) => to_posix(rel),
        Err(_) => to_posix(&resolved),
    }
}

/// Format `path` relative to `root` using forward slashes, or full POSIX path if outside `root`.
pub fn display_path(path: &Path, root: &Path) -> String {
    display_path_resolved(path, &resolve_path(root))
}

pub fn keep_entry(entry: &Entry, min_lines: usize, min_nodes: usize) -> bool {
    let lines = entry.end_line.saturating_sub(entry.start_line) + 1;
    lines >= min_lines && entry.nodes >= min_nodes
}

/// Read UTF-8 with lossy replacement and Python universal-newline normalization (`\r\n` and `\r` -> `\n`).
pub fn read_source_text(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let lossy = String::from_utf8_lossy(&bytes);
    if !lossy.contains('\r') {
        return Ok(lossy.into_owned());
    }
    let mut out = String::with_capacity(lossy.len());
    let mut chars = lossy.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn scan_one(
    path: &Path,
    root_resolved: &Path,
    min_lines: usize,
    min_nodes: usize,
) -> (Vec<Entry>, Option<String>) {
    let Some(language) = language_of(path) else {
        return (Vec::new(), None);
    };
    let Ok(source) = read_source_text(path) else {
        return (Vec::new(), None);
    };
    let file = display_path_resolved(path, root_resolved);
    let posix_path = to_posix(path);
    let (found, warning) = entries_in_source(language, &source, &posix_path, &file);
    let kept = found
        .into_iter()
        .filter(|entry| keep_entry(entry, min_lines, min_nodes))
        .collect();
    let warn = warning.map(|msg| format!("{file}:{msg}"));
    (kept, warn)
}

pub fn scan_files(
    files: &[PathBuf],
    root: &Path,
    min_lines: usize,
    min_nodes: usize,
) -> (Vec<Entry>, Vec<String>) {
    let root_resolved = resolve_path(root);
    let per_file: Vec<(Vec<Entry>, Option<String>)> = files
        .par_iter()
        .map(|path| scan_one(path, &root_resolved, min_lines, min_nodes))
        .collect();
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (found, warn) in per_file {
        entries.extend(found);
        if let Some(msg) = warn {
            warnings.push(msg);
        }
    }
    (entries, warnings)
}

fn same_span(left: &Entry, right: &Entry) -> bool {
    left.file == right.file
        && left.start_line == right.start_line
        && left.end_line == right.end_line
}

fn make_duplicate(language: &str, left: &Entry, right: &Entry, score: f64) -> Duplicate {
    Duplicate {
        score,
        language: language.to_string(),
        left: Span {
            file: left.file.clone(),
            start_line: left.start_line,
            end_line: left.end_line,
        },
        right: Span {
            file: right.file.clone(),
            start_line: right.start_line,
            end_line: right.end_line,
        },
        left_nodes: left.nodes,
        right_nodes: right.nodes,
    }
}

fn find_duplicates_fallback(language: &str, group: &[&Entry], threshold: f64) -> Vec<Duplicate> {
    (0..group.len())
        .into_par_iter()
        .flat_map(|i| {
            let left = group[i];
            let mut local = Vec::new();
            for right in &group[i + 1..] {
                if same_span(left, right) {
                    continue;
                }
                let score = jaccard(&left.fingerprints, &right.fingerprints);
                if score >= threshold {
                    local.push(make_duplicate(language, left, right, score));
                }
            }
            local
        })
        .collect()
}

fn min_overlap_for_len(len: usize, threshold: f64) -> usize {
    if len == 0 {
        return 1;
    }
    let mut k = ((threshold * (len as f64)).floor() as usize).clamp(1, len);
    while k < len && (k as f64) / (len as f64) < threshold {
        k += 1;
    }
    while k > 1 && ((k - 1) as f64) / (len as f64) >= threshold {
        k -= 1;
    }
    k
}

fn encode_group(group: &[&Entry]) -> (Vec<Vec<u32>>, usize) {
    let mut intern: HashMap<&str, u32> = HashMap::new();
    let mut freq: Vec<usize> = Vec::new();
    let mut raw_tokens: Vec<Vec<u32>> = Vec::with_capacity(group.len());

    for entry in group {
        let mut ids = Vec::with_capacity(entry.fingerprints.len());
        for fp in &entry.fingerprints {
            let id = match intern.get(fp.as_str()) {
                Some(&id) => {
                    freq[id as usize] += 1;
                    id
                }
                None => {
                    let id = u32::try_from(freq.len()).expect("fingerprint vocabulary fits u32");
                    intern.insert(fp.as_str(), id);
                    freq.push(1);
                    id
                }
            };
            ids.push(id);
        }
        raw_tokens.push(ids);
    }

    let vocab = freq.len();
    let mut order: Vec<u32> = (0..vocab as u32).collect();
    order.sort_unstable_by_key(|&id| (freq[id as usize], id));
    let mut remap = vec![0u32; vocab];
    for (new_id, &old_id) in order.iter().enumerate() {
        remap[old_id as usize] = new_id as u32;
    }

    for ids in &mut raw_tokens {
        for id in ids.iter_mut() {
            *id = remap[*id as usize];
        }
        ids.sort_unstable();
    }
    (raw_tokens, vocab)
}

fn intersect_at_least(a: &[u32], b: &[u32], min_needed: usize) -> Option<usize> {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut shared = 0usize;
    while i < a.len() && j < b.len() {
        if shared + (a.len() - i) < min_needed || shared + (b.len() - j) < min_needed {
            return None;
        }
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    (shared >= min_needed).then_some(shared)
}

struct CandidateScratch {
    seen: Vec<u32>,
    epoch: u32,
    candidates: Vec<usize>,
}

impl CandidateScratch {
    fn new(len: usize) -> Self {
        Self {
            seen: vec![0u32; len],
            epoch: 0,
            candidates: Vec::new(),
        }
    }

    fn next_epoch(&mut self) -> u32 {
        if self.epoch == u32::MAX {
            self.seen.fill(0);
            self.epoch = 1;
        } else {
            self.epoch += 1;
        }
        self.candidates.clear();
        self.epoch
    }
}

fn find_duplicates_indexed(language: &str, group: &[&Entry], threshold: f64) -> Vec<Duplicate> {
    let (tokens, vocab) = encode_group(group);
    let min_k: Vec<usize> = tokens
        .iter()
        .map(|toks| min_overlap_for_len(toks.len(), threshold))
        .collect();
    let prefix_lens: Vec<usize> = tokens
        .iter()
        .zip(&min_k)
        .map(|(toks, &k)| {
            if toks.is_empty() {
                0
            } else {
                toks.len() - k + 1
            }
        })
        .collect();

    let mut postings: Vec<Vec<usize>> = vec![Vec::new(); vocab];
    for (idx, (toks, &plen)) in tokens.iter().zip(&prefix_lens).enumerate() {
        for &tok in &toks[..plen] {
            postings[tok as usize].push(idx);
        }
    }

    let n = group.len();
    (0..n)
        .into_par_iter()
        .map_init(
            || CandidateScratch::new(n),
            |scratch, i| {
                let len_i = tokens[i].len();
                if len_i == 0 {
                    return Vec::new();
                }
                let epoch = scratch.next_epoch();
                for &tok in &tokens[i][..prefix_lens[i]] {
                    let list = &postings[tok as usize];
                    let start = list.partition_point(|&idx| idx <= i);
                    for &j in &list[start..] {
                        if scratch.seen[j] != epoch {
                            scratch.seen[j] = epoch;
                            scratch.candidates.push(j);
                        }
                    }
                }
                if scratch.candidates.is_empty() {
                    return Vec::new();
                }
                scratch.candidates.sort_unstable();
                let left = group[i];
                let mut local = Vec::new();
                for &j in &scratch.candidates {
                    let len_j = tokens[j].len();
                    let (small, big) = if len_i <= len_j {
                        (len_i, len_j)
                    } else {
                        (len_j, len_i)
                    };
                    if (small as f64) / (big as f64) < threshold {
                        continue;
                    }
                    let right = group[j];
                    if same_span(left, right) {
                        continue;
                    }
                    let min_needed = min_k[i].max(min_k[j]);
                    let Some(shared) = intersect_at_least(&tokens[i], &tokens[j], min_needed)
                    else {
                        continue;
                    };
                    let union = len_i + len_j - shared;
                    let score = shared as f64 / union as f64;
                    if score >= threshold {
                        local.push(make_duplicate(language, left, right, score));
                    }
                }
                local
            },
        )
        .flatten()
        .collect()
}

/// Pair forms of the same language. Each unordered pair is reported once, sorted by score descending.
pub fn find_duplicates(entries: &[Entry], threshold: f64) -> Vec<Duplicate> {
    if threshold > 1.0 {
        return Vec::new();
    }
    let mut by_language: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        by_language
            .entry(entry.language.as_str())
            .or_default()
            .push(entry);
    }

    let use_indexed = threshold > 0.0;
    let mut found = Vec::new();
    for (language, group) in by_language {
        if group.len() < 2 {
            continue;
        }
        let mut pairs = if use_indexed {
            find_duplicates_indexed(language, &group, threshold)
        } else {
            find_duplicates_fallback(language, &group, threshold)
        };
        found.append(&mut pairs);
    }

    found.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.language.cmp(&b.language))
            .then_with(|| a.left.file.cmp(&b.left.file))
            .then_with(|| a.left.start_line.cmp(&b.left.start_line))
            .then_with(|| a.right.file.cmp(&b.right.file))
            .then_with(|| a.right.start_line.cmp(&b.right.start_line))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_entry(
        file: &str,
        start: usize,
        end: usize,
        names: &[&str],
        lang: &str,
        nodes: usize,
    ) -> Entry {
        Entry {
            language: lang.to_string(),
            file: file.to_string(),
            start_line: start,
            end_line: end,
            nodes,
            fingerprints: names.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn skips_same_span_and_sorts_highest_score_first() {
        assert!(
            find_duplicates(
                &[
                    make_entry("a.py", 1, 4, &["q"], "python", 20),
                    make_entry("a.py", 1, 4, &["q"], "python", 20),
                ],
                0.0,
            )
            .is_empty()
        );

        let exact = [
            make_entry("a.py", 1, 4, &["only"], "python", 20),
            make_entry("b.py", 1, 4, &["only"], "python", 20),
        ];
        let partial = [
            make_entry("a.rs", 1, 4, &["x", "y"], "rust", 20),
            make_entry("b.rs", 10, 14, &["x", "z"], "rust", 20),
        ];
        let mut all = Vec::new();
        all.extend(partial);
        all.extend(exact);
        let found = find_duplicates(&all, 0.3);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].score, 1.0);
        assert!((found[1].score - (1.0 / 3.0)).abs() < 1e-12);
    }

    #[test]
    fn keep_checks_line_and_node_minimums() {
        let e = Entry {
            language: "python".into(),
            file: "a.py".into(),
            start_line: 1,
            end_line: 4,
            nodes: 20,
            fingerprints: HashSet::from(["q".into()]),
        };
        assert!(keep_entry(&e, 4, 20));
        assert!(!keep_entry(&e, 5, 20));
        assert!(!keep_entry(&e, 4, 21));
    }

    fn brute_force_reference(entries: &[Entry], threshold: f64) -> Vec<Duplicate> {
        let mut by_language: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for entry in entries {
            by_language
                .entry(entry.language.as_str())
                .or_default()
                .push(entry);
        }
        let mut found = Vec::new();
        for (language, group) in by_language {
            for (idx, left) in group.iter().enumerate() {
                for right in &group[idx + 1..] {
                    if same_span(left, right) {
                        continue;
                    }
                    let score = jaccard(&left.fingerprints, &right.fingerprints);
                    if score >= threshold {
                        found.push(make_duplicate(language, left, right, score));
                    }
                }
            }
        }
        found.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.language.cmp(&b.language))
                .then_with(|| a.left.file.cmp(&b.left.file))
                .then_with(|| a.left.start_line.cmp(&b.left.start_line))
                .then_with(|| a.right.file.cmp(&b.right.file))
                .then_with(|| a.right.start_line.cmp(&b.right.start_line))
        });
        found
    }

    #[test]
    fn indexed_join_matches_brute_force_across_thresholds() {
        let entries = vec![
            make_entry("a.ts", 1, 5, &[], "typescript", 10),
            make_entry("b.ts", 1, 5, &[], "typescript", 10),
            make_entry("c.ts", 1, 5, &["a"], "typescript", 12),
            make_entry("c.ts", 1, 5, &["a"], "typescript", 12),
            make_entry("d.ts", 2, 6, &["a", "b"], "typescript", 14),
            make_entry("e.ts", 3, 9, &["a", "b", "c"], "typescript", 16),
            make_entry("f.ts", 4, 12, &["b", "c", "d", "e"], "typescript", 18),
            make_entry(
                "g.ts",
                5,
                15,
                &["x", "y", "a", "b", "c", "d", "e", "f", "g", "h"],
                "typescript",
                25,
            ),
            make_entry(
                "h.ts",
                6,
                16,
                &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"],
                "typescript",
                25,
            ),
            make_entry("p.py", 1, 4, &["a", "b"], "python", 12),
            make_entry("q.py", 2, 5, &["a", "z"], "python", 12),
        ];

        for threshold in [0.0, 0.1, 0.3, 0.5, 1.0 / 3.0, 0.8, 1.0, 1.1] {
            let actual = find_duplicates(&entries, threshold);
            let expected = brute_force_reference(&entries, threshold);
            assert_eq!(actual, expected, "mismatch at threshold {threshold}");
        }
    }
}
