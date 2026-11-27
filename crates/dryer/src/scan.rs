//! Read source files and pair forms that meet the similarity threshold.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::Hash;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use ub_runtime::FileStamp;

use super::cache::{CachedFile, DryerCache, HashedEntry};
use super::discover::{language_of, resolve_path, to_posix};
use super::extract::entries_in_source;
use super::model::{Duplicate, Entry, Span};
use super::shape::jaccard;

fn relative_display(resolved: &Path, root_resolved: &Path) -> String {
    match resolved.strip_prefix(root_resolved) {
        Ok(rel) => to_posix(rel),
        Err(_) => to_posix(resolved),
    }
}

/// Format `path` relative to `root` using forward slashes, or full POSIX path if outside `root`.
pub fn display_path(path: &Path, root: &Path) -> String {
    relative_display(&resolve_path(path), &resolve_path(root))
}

fn keep_span(
    start_line: usize,
    end_line: usize,
    nodes: usize,
    min_lines: usize,
    min_nodes: usize,
) -> bool {
    let lines = end_line.saturating_sub(start_line) + 1;
    lines >= min_lines && nodes >= min_nodes
}

pub fn keep_entry(entry: &Entry, min_lines: usize, min_nodes: usize) -> bool {
    keep_span(
        entry.start_line,
        entry.end_line,
        entry.nodes,
        min_lines,
        min_nodes,
    )
}

pub fn keep_hashed_entry(entry: &HashedEntry, min_lines: usize, min_nodes: usize) -> bool {
    keep_span(
        entry.start_line,
        entry.end_line,
        entry.nodes,
        min_lines,
        min_nodes,
    )
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub wrote_cache: bool,
}

struct MissTask {
    idx: usize,
    path: PathBuf,
    key: String,
    language: &'static str,
    display_file: String,
    stamp: FileStamp,
}

struct RawFileSlot {
    key: String,
    cached: CachedFile,
}

fn parse_miss(task: &MissTask) -> Option<(usize, RawFileSlot)> {
    let source = read_source_text(&task.path).ok()?;
    let (raw_entries, warning) =
        entries_in_source(task.language, &source, &task.key, &task.display_file);
    let hashed_entries = raw_entries.iter().map(HashedEntry::from_entry).collect();
    Some((
        task.idx,
        RawFileSlot {
            key: task.key.clone(),
            cached: CachedFile {
                stamp: task.stamp,
                language: task.language.to_string(),
                display_file: task.display_file.clone(),
                raw_entries: hashed_entries,
                warning,
            },
        },
    ))
}

pub(crate) fn scan_hashed_with_cache_inner(
    files: &[PathBuf],
    root: &Path,
    min_lines: usize,
    min_nodes: usize,
    use_cache: bool,
    cache_path_override: Option<&Path>,
    paths_resolved: bool,
) -> (Vec<HashedEntry>, Vec<String>, ScanCacheStats) {
    if files.is_empty() {
        return (Vec::new(), Vec::new(), ScanCacheStats::default());
    }

    let root_resolved = resolve_path(root);
    let resolved_cache_path;
    let cache_path = if use_cache {
        Some(match cache_path_override {
            Some(path) => path,
            None => {
                resolved_cache_path = DryerCache::repo_path(&root_resolved);
                resolved_cache_path.as_path()
            }
        })
    } else {
        None
    };

    let mut prior_cache = cache_path.map(DryerCache::load_from).unwrap_or_default();
    let zero_stamp = FileStamp {
        mtime_ns: 0,
        size: 0,
        extra_mtime_ns: 0,
        extra_size: 0,
    };

    struct PreparedEntry {
        resolved: PathBuf,
        key: String,
        language: &'static str,
        display_file: String,
        stamp: FileStamp,
    }

    let prepared: Vec<Option<PreparedEntry>> = files
        .par_iter()
        .map(|path| {
            let language = language_of(path)?;
            let resolved = if paths_resolved {
                path.clone()
            } else {
                resolve_path(path)
            };
            let stamp = if use_cache {
                FileStamp::from_paths(&resolved, 0, 0)?
            } else {
                zero_stamp
            };
            let key = to_posix(&resolved);
            let display_file = relative_display(&resolved, &root_resolved);
            Some(PreparedEntry {
                resolved,
                key,
                language,
                display_file,
                stamp,
            })
        })
        .collect();

    let mut raw_slots: Vec<Option<RawFileSlot>> = (0..files.len()).map(|_| None).collect();
    let mut hit_slots: Vec<(usize, String)> = Vec::new();
    let mut misses = Vec::new();

    for (idx, meta) in prepared.into_iter().enumerate() {
        let Some(PreparedEntry {
            resolved,
            key,
            language,
            display_file,
            stamp,
        }) = meta
        else {
            continue;
        };
        if use_cache
            && let Some(cached) = prior_cache.get(&key)
            && cached.stamp == stamp
            && cached.language == language
            && cached.display_file == display_file
        {
            hit_slots.push((idx, key));
        } else {
            misses.push(MissTask {
                idx,
                path: resolved,
                key,
                language,
                display_file,
                stamp,
            });
        }
    }

    let hits = hit_slots.len();
    let miss_count = misses.len();
    if miss_count == 0 {
        for (idx, key) in hit_slots {
            if let Some(cached) = prior_cache.remove(&key) {
                raw_slots[idx] = Some(RawFileSlot { key, cached });
            } else if let Some(cached) = raw_slots
                .iter()
                .flatten()
                .find(|slot| slot.key == key)
                .map(|slot| slot.cached.clone())
            {
                raw_slots[idx] = Some(RawFileSlot { key, cached });
            }
        }
    } else {
        for (idx, key) in hit_slots {
            if let Some(cached) = prior_cache.get(&key) {
                raw_slots[idx] = Some(RawFileSlot {
                    key,
                    cached: cached.clone(),
                });
            }
        }
    }

    let parsed_misses: Vec<(usize, RawFileSlot)> =
        misses.par_iter().filter_map(parse_miss).collect();
    let any_parsed = !parsed_misses.is_empty();
    for (idx, slot) in parsed_misses {
        raw_slots[idx] = Some(slot);
    }

    let mut wrote_cache = false;
    if let Some(path) = cache_path
        && miss_count > 0
        && any_parsed
    {
        let active: Vec<(String, CachedFile)> = raw_slots
            .iter()
            .flatten()
            .map(|slot| (slot.key.clone(), slot.cached.clone()))
            .collect();
        prior_cache.merge_and_save_to(path, active);
        wrote_cache = true;
    }

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for slot in raw_slots.into_iter().flatten() {
        let display_file = slot.cached.display_file;
        if let Some(msg) = slot.cached.warning {
            warnings.push(format!("{display_file}:{msg}"));
        }
        for entry in slot.cached.raw_entries {
            if keep_hashed_entry(&entry, min_lines, min_nodes) {
                entries.push(entry);
            }
        }
    }

    let stats = ScanCacheStats {
        hits,
        misses: miss_count,
        wrote_cache,
    };
    (entries, warnings, stats)
}

/// Scan `files` into compact `HashedEntry` records using the per-repository cache when enabled.
pub fn scan_hashed_with_cache(
    files: &[PathBuf],
    root: &Path,
    min_lines: usize,
    min_nodes: usize,
    use_cache: bool,
) -> (Vec<HashedEntry>, Vec<String>) {
    let (entries, warnings, _) =
        scan_hashed_with_cache_inner(files, root, min_lines, min_nodes, use_cache, None, false);
    (entries, warnings)
}

pub(crate) fn scan_and_find_duplicates_resolved(
    files: &[PathBuf],
    root: &Path,
    min_lines: usize,
    min_nodes: usize,
    threshold: f64,
    use_cache: bool,
) -> (usize, Vec<Duplicate>, Vec<String>) {
    let (entries, warnings, _) =
        scan_hashed_with_cache_inner(files, root, min_lines, min_nodes, use_cache, None, true);
    let count = entries.len();
    let candidates = find_duplicates_hashed(&entries, threshold);
    (count, candidates, warnings)
}

pub fn scan_files(
    files: &[PathBuf],
    root: &Path,
    min_lines: usize,
    min_nodes: usize,
) -> (Vec<Entry>, Vec<String>) {
    if files.is_empty() {
        return (Vec::new(), Vec::new());
    }
    struct ParsedUncached {
        display_file: String,
        raw_entries: Vec<Entry>,
        warning: Option<String>,
    }

    let root_resolved = resolve_path(root);
    let parsed: Vec<Option<ParsedUncached>> = files
        .par_iter()
        .map(|path| {
            let language = language_of(path)?;
            let resolved = resolve_path(path);
            let display_file = relative_display(&resolved, &root_resolved);
            let posix_path = to_posix(&resolved);
            let source = read_source_text(&resolved).ok()?;
            let (raw_entries, warning) =
                entries_in_source(language, &source, &posix_path, &display_file);
            Some(ParsedUncached {
                display_file,
                raw_entries,
                warning,
            })
        })
        .collect();

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for item in parsed.into_iter().flatten() {
        if let Some(msg) = item.warning {
            warnings.push(format!("{}:{msg}", item.display_file));
        }
        for entry in item.raw_entries {
            if keep_entry(&entry, min_lines, min_nodes) {
                entries.push(entry);
            }
        }
    }
    (entries, warnings)
}

trait FingerprintItem: Sync {
    fn language(&self) -> &str;
    fn file(&self) -> &str;
    fn start_line(&self) -> usize;
    fn end_line(&self) -> usize;
    fn nodes(&self) -> usize;
}

impl FingerprintItem for Entry {
    fn language(&self) -> &str {
        &self.language
    }

    fn file(&self) -> &str {
        &self.file
    }

    fn start_line(&self) -> usize {
        self.start_line
    }

    fn end_line(&self) -> usize {
        self.end_line
    }

    fn nodes(&self) -> usize {
        self.nodes
    }
}

impl FingerprintItem for HashedEntry {
    fn language(&self) -> &str {
        &self.language
    }

    fn file(&self) -> &str {
        &self.file
    }

    fn start_line(&self) -> usize {
        self.start_line
    }

    fn end_line(&self) -> usize {
        self.end_line
    }

    fn nodes(&self) -> usize {
        self.nodes
    }
}

fn same_span<T: FingerprintItem>(left: &T, right: &T) -> bool {
    left.file() == right.file()
        && left.start_line() == right.start_line()
        && left.end_line() == right.end_line()
}

fn make_duplicate<T: FingerprintItem>(
    language: &str,
    left: &T,
    right: &T,
    score: f64,
) -> Duplicate {
    Duplicate {
        score,
        language: language.to_string(),
        left: Span {
            file: left.file().to_string(),
            start_line: left.start_line(),
            end_line: left.end_line(),
        },
        right: Span {
            file: right.file().to_string(),
            start_line: right.start_line(),
            end_line: right.end_line(),
        },
        left_nodes: left.nodes(),
        right_nodes: right.nodes(),
    }
}

fn sorted_u128_jaccard(left: &[u128], right: &[u128]) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 0.0;
    }
    let mut i = 0usize;
    let mut j = 0usize;
    let mut shared = 0usize;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = left.len() + right.len() - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

fn find_duplicates_fallback_core<T: FingerprintItem, F>(
    language: &str,
    group: &[&T],
    threshold: f64,
    score_fn: &F,
) -> Vec<Duplicate>
where
    F: Fn(&T, &T) -> f64 + Sync,
{
    (0..group.len())
        .into_par_iter()
        .flat_map(|i| {
            let left = group[i];
            let mut local = Vec::new();
            for right in &group[i + 1..] {
                if same_span(left, *right) {
                    continue;
                }
                let score = score_fn(left, *right);
                if score >= threshold {
                    local.push(make_duplicate(language, left, *right, score));
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

fn encode_group_with<'a, T, K, I, E>(group: &[&'a T], keys_of: &E) -> (Vec<Vec<u32>>, usize)
where
    K: Eq + Hash,
    I: IntoIterator<Item = K>,
    E: Fn(&'a T) -> I,
{
    let mut intern: HashMap<K, u32> = HashMap::new();
    let mut freq: Vec<usize> = Vec::new();
    let mut raw_tokens: Vec<Vec<u32>> = Vec::with_capacity(group.len());

    for &entry in group {
        let mut ids = Vec::new();
        for fp in keys_of(entry) {
            let id = match intern.get(&fp) {
                Some(&id) => {
                    freq[id as usize] += 1;
                    id
                }
                None => {
                    let id = u32::try_from(freq.len()).expect("fingerprint vocabulary fits u32");
                    intern.insert(fp, id);
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

fn find_duplicates_indexed_core<'a, T, K, I, E>(
    language: &str,
    group: &[&'a T],
    threshold: f64,
    keys_of: &E,
) -> Vec<Duplicate>
where
    T: FingerprintItem,
    K: Eq + Hash,
    I: IntoIterator<Item = K>,
    E: Fn(&'a T) -> I,
{
    let (tokens, vocab) = encode_group_with(group, keys_of);
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

fn find_duplicates_core<'a, T, K, I, E, F>(
    entries: &'a [T],
    threshold: f64,
    keys_of: E,
    fallback_score: F,
) -> Vec<Duplicate>
where
    T: FingerprintItem,
    K: Eq + Hash,
    I: IntoIterator<Item = K>,
    E: Fn(&'a T) -> I,
    F: Fn(&T, &T) -> f64 + Sync,
{
    if threshold > 1.0 {
        return Vec::new();
    }
    let mut by_language: BTreeMap<&str, Vec<&'a T>> = BTreeMap::new();
    for entry in entries {
        by_language.entry(entry.language()).or_default().push(entry);
    }

    let use_indexed = threshold > 0.0;
    let mut found = Vec::new();
    for (language, group) in by_language {
        if group.len() < 2 {
            continue;
        }
        let mut pairs = if use_indexed {
            find_duplicates_indexed_core(language, &group, threshold, &keys_of)
        } else {
            find_duplicates_fallback_core(language, &group, threshold, &fallback_score)
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

/// Pair forms of the same language. Each unordered pair is reported once, sorted by score descending.
pub fn find_duplicates(entries: &[Entry], threshold: f64) -> Vec<Duplicate> {
    find_duplicates_core(
        entries,
        threshold,
        |entry| entry.fingerprints.iter().map(String::as_str),
        |left, right| jaccard(&left.fingerprints, &right.fingerprints),
    )
}

/// Pair compact `HashedEntry` forms of the same language using 128-bit fingerprint hashes.
pub fn find_duplicates_hashed(entries: &[HashedEntry], threshold: f64) -> Vec<Duplicate> {
    find_duplicates_core(
        entries,
        threshold,
        |entry| entry.fingerprints.iter().copied(),
        |left, right| sorted_u128_jaccard(&left.fingerprints, &right.fingerprints),
    )
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
            for (idx, &left) in group.iter().enumerate() {
                for &right in &group[idx + 1..] {
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
    fn hashed_join_matches_string_join_and_brute_force_across_thresholds() {
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
        let hashed: Vec<HashedEntry> = entries.iter().map(HashedEntry::from_entry).collect();

        for threshold in [0.0, 0.1, 0.3, 0.5, 1.0 / 3.0, 0.8, 1.0, 1.1] {
            let expected = brute_force_reference(&entries, threshold);
            assert_eq!(find_duplicates(&entries, threshold), expected);
            assert_eq!(
                find_duplicates_hashed(&hashed, threshold),
                expected,
                "hashed mismatch at threshold {threshold}"
            );
        }
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

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "dryer-scan-cache-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_clojure_fixture(repo: &Path) -> Vec<PathBuf> {
        let left = repo.join("left.cljc");
        let right = repo.join("right.cljc");
        let small = repo.join("small.cljc");
        std::fs::write(
            &left,
            "(ns sample.left)\n\n(defn alpha [xs]\n  (let [ys (filter odd? xs)]\n    (map inc ys)))\n",
        )
        .unwrap();
        std::fs::write(
            &right,
            "(ns sample.right)\n\n(defn beta [items]\n  (let [kept (filter even? items)]\n    (map dec kept)))\n(defn broken [x]\n  \"unclosed\n",
        )
        .unwrap();
        std::fs::write(&small, "(defn tiny [x]\n  (inc x))\n").unwrap();
        vec![left, right, small]
    }

    fn hashed_from_uncached(entries: &[Entry]) -> Vec<HashedEntry> {
        entries.iter().map(HashedEntry::from_entry).collect()
    }

    #[test]
    fn warm_cache_matches_uncached_and_avoids_writes_when_hits_only() {
        let dir = TempDirGuard::new("warm-hit");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let files = write_clojure_fixture(&repo);
        let cache_file = dir.path().join("dryer.cache");

        let (uncached_entries, uncached_warnings) = scan_files(&files, &repo, 3, 8);
        let uncached_hashed = hashed_from_uncached(&uncached_entries);
        let uncached_dups = find_duplicates(&uncached_entries, 0.82);
        assert_eq!(uncached_dups.len(), 1);
        assert_eq!(uncached_warnings.len(), 1);
        assert!(uncached_warnings[0].starts_with("right.cljc:"));

        let (cold_entries, cold_warnings, cold_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(cold_stats.hits, 0);
        assert_eq!(cold_stats.misses, 3);
        assert!(cold_stats.wrote_cache);
        assert!(cache_file.is_file());
        assert_eq!(cold_entries, uncached_hashed);
        assert_eq!(cold_warnings, uncached_warnings);
        assert_eq!(find_duplicates_hashed(&cold_entries, 0.82), uncached_dups);

        let stamp_before = ub_runtime::file_mtime_size(&cache_file).unwrap();
        let bytes_before = std::fs::read(&cache_file).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let (warm_entries, warm_warnings, warm_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(warm_stats.hits, 3);
        assert_eq!(warm_stats.misses, 0);
        assert!(!warm_stats.wrote_cache);
        assert_eq!(
            ub_runtime::file_mtime_size(&cache_file).unwrap(),
            stamp_before
        );
        assert_eq!(std::fs::read(&cache_file).unwrap(), bytes_before);
        assert_eq!(warm_entries, uncached_hashed);
        assert_eq!(warm_warnings, uncached_warnings);
        assert_eq!(find_duplicates_hashed(&warm_entries, 0.82), uncached_dups);
    }

    #[test]
    fn warm_cache_preserves_accuracy_across_min_lines_and_min_nodes() {
        let dir = TempDirGuard::new("thresholds");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let files = write_clojure_fixture(&repo);
        let cache_file = dir.path().join("dryer.cache");

        let (strict_entries, _, strict_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 10, 100, true, Some(&cache_file), false);
        assert!(strict_entries.is_empty());
        assert!(strict_stats.wrote_cache);

        let (relaxed_entries, relaxed_warnings, relaxed_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 2, 3, true, Some(&cache_file), false);
        assert_eq!(relaxed_stats.hits, 3);
        assert_eq!(relaxed_stats.misses, 0);
        assert!(!relaxed_stats.wrote_cache);

        let (expected_entries, expected_warnings) = scan_files(&files, &repo, 2, 3);
        assert_eq!(relaxed_entries, hashed_from_uncached(&expected_entries));
        assert_eq!(relaxed_warnings, expected_warnings);
        assert!(relaxed_entries.iter().any(|e| e.file == "small.cljc"));

        let (mid_entries, _, mid_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(mid_stats.hits, 3);
        assert_eq!(mid_stats.misses, 0);
        assert!(!mid_stats.wrote_cache);
        assert_eq!(
            mid_entries,
            hashed_from_uncached(&scan_files(&files, &repo, 3, 8).0)
        );
    }

    #[test]
    fn cache_invalidates_on_source_edit_and_tolerates_corrupt_cache() {
        let dir = TempDirGuard::new("invalidate-corrupt");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let files = write_clojure_fixture(&repo);
        let cache_file = dir.path().join("dryer.cache");

        let (_, _, first_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(first_stats.misses, 3);
        assert!(first_stats.wrote_cache);

        // Modify one file so both size and mtime change.
        std::fs::write(
            &files[0],
            "(ns sample.left)\n\n(defn alpha [xs]\n  (let [ys (filter odd? xs)]\n    (vec (map inc ys))))\n",
        )
        .unwrap();

        let (edited_entries, edited_warnings, edit_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(edit_stats.hits, 2);
        assert_eq!(edit_stats.misses, 1);
        assert!(edit_stats.wrote_cache);
        let (expected_entries, expected_warnings) = scan_files(&files, &repo, 3, 8);
        assert_eq!(edited_entries, hashed_from_uncached(&expected_entries));
        assert_eq!(edited_warnings, expected_warnings);

        // Corrupt cache on disk; scan must recover cleanly without error.
        std::fs::write(&cache_file, b"UBC1\x02\x00\x00\x00corrupt").unwrap();
        let (recovered_entries, recovered_warnings, corrupt_stats) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(corrupt_stats.hits, 0);
        assert_eq!(corrupt_stats.misses, 3);
        assert!(corrupt_stats.wrote_cache);
        assert_eq!(recovered_entries, hashed_from_uncached(&expected_entries));
        assert_eq!(recovered_warnings, expected_warnings);

        // Subsequent warm run hits the repaired cache.
        let (_, _, after_repair) =
            scan_hashed_with_cache_inner(&files, &repo, 3, 8, true, Some(&cache_file), false);
        assert_eq!(after_repair.hits, 3);
        assert_eq!(after_repair.misses, 0);
        assert!(!after_repair.wrote_cache);
    }

    #[test]
    fn public_scan_hashed_with_cache_uses_env_cache_dir_in_subprocess() {
        const SUBPROC: &str = "DRYER_SCAN_CACHE_SUBPROC";
        if std::env::var_os(SUBPROC).is_some() {
            let dir = TempDirGuard::new("subproc-repo");
            let files = write_clojure_fixture(dir.path());
            let cache_dir = ub_runtime::cache_dir();
            let expected_cache = DryerCache::repo_path(&resolve_path(dir.path()));
            assert!(expected_cache.starts_with(&cache_dir));

            let (uncached, _) = scan_hashed_with_cache(&files, dir.path(), 3, 8, false);
            assert!(!expected_cache.exists());

            let (cold, _) = scan_hashed_with_cache(&files, dir.path(), 3, 8, true);
            assert_eq!(cold, uncached);
            assert!(expected_cache.is_file());

            let stamp = ub_runtime::file_mtime_size(&expected_cache).unwrap();
            let (warm, _) = scan_hashed_with_cache(&files, dir.path(), 3, 8, true);
            assert_eq!(warm, uncached);
            assert_eq!(ub_runtime::file_mtime_size(&expected_cache).unwrap(), stamp);
            return;
        }

        let dir = TempDirGuard::new("subproc-cache");
        let exe = std::env::current_exe().expect("current test exe");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "scan::tests::public_scan_hashed_with_cache_uses_env_cache_dir_in_subprocess",
                "--nocapture",
            ])
            .env(SUBPROC, "1")
            .env("UB_TOOLS_CACHE_DIR", dir.path())
            .output()
            .expect("spawn scan cache subproc");
        assert!(
            out.status.success(),
            "subproc failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
