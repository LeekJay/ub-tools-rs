//! Decide which lines the tests already exercise.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::path_util::{resolve_path, to_posix};

static DOCTYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<!DOCTYPE[^>]*>").expect("valid regex"));

/// Representation of Swift coverage regions and xccov summaries needed by mutator.
#[derive(Debug, Clone, Default)]
pub struct SwiftCoverageRecord {
    pub regions: Vec<(usize, usize, u64)>,
    pub summaries: Vec<(usize, usize, usize)>,
}

impl SwiftCoverageRecord {
    /// True when the innermost region containing `line` has count > 0.
    pub fn line_covered(&self, line: usize) -> bool {
        let innermost = self
            .regions
            .iter()
            .filter(|&&(start, end, _)| start <= line && line <= end)
            .min_by_key(|&&(start, end, _)| (end - start, std::cmp::Reverse(start)));
        innermost.is_some_and(|&(_, _, count)| count > 0)
    }
}

/// Go coverage segments per file key (`start_line, end_line, statements, hits`).
pub type GoProfileMap = HashMap<String, Vec<(usize, usize, usize, u64)>>;

/// Abstract coverage bundle provider backed by `crapper-core`.
pub trait CoverageProvider {
    fn normalize_path(&self, path: &str) -> String;
    fn go_import_name(&self, source_path: &Path, root: &Path) -> Option<String>;
    fn load_go_profile(&self, root: &Path) -> Option<GoProfileMap>;
    fn load_form_html(&self, root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>>;
    fn load_lcov(&self, root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>>;
    fn lookup_swift(&self, root: &Path, source_path: &str) -> Option<SwiftCoverageRecord>;
}

fn candidates<F>(normalize: F, source_path: &str) -> Vec<String>
where
    F: Fn(&str) -> String,
{
    let absolute = if source_path.is_empty() {
        String::new()
    } else {
        normalize(&to_posix(&resolve_path(Path::new(source_path))))
    };
    let mut out = Vec::new();
    for value in [normalize(source_path), absolute] {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn exact<'a, V>(normalized: &HashMap<String, &'a V>, cands: &[String]) -> Option<&'a V> {
    for candidate in cands {
        if let Some(&value) = normalized.get(candidate) {
            return Some(value);
        }
    }
    None
}

fn path_parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|p| !p.is_empty()).collect()
}

fn as_suffix(longer: &[&str], shorter: &[&str]) -> Option<usize> {
    if longer.len() >= shorter.len() && &longer[longer.len() - shorter.len()..] == shorter {
        Some(longer.len() - shorter.len())
    } else {
        None
    }
}

fn suffix_rank(key_parts: &[&str], source: &[&str]) -> Option<(u8, usize)> {
    if source.is_empty() || key_parts.is_empty() {
        return None;
    }
    if let Some(extra) = as_suffix(key_parts, source) {
        return Some((0, extra));
    }
    as_suffix(source, key_parts).map(|missing| (1, missing))
}

fn best_rank_for_key(key_parts: &[&str], sources: &[Vec<&str>]) -> Option<(u8, usize)> {
    sources
        .iter()
        .filter_map(|src| suffix_rank(key_parts, src))
        .min()
}

fn best_value<'a, V>(normalized: &HashMap<String, &'a V>, sources: &[Vec<&str>]) -> Option<&'a V> {
    let mut best_rank: Option<(u8, usize)> = None;
    let mut best: Vec<(Vec<&str>, &'a V)> = Vec::new();
    for (key, value) in normalized {
        let key_parts = path_parts(key);
        let Some(rank) = best_rank_for_key(&key_parts, sources) else {
            continue;
        };
        match best_rank {
            None => {
                best_rank = Some(rank);
                best = vec![(key_parts, *value)];
            }
            Some(cur) if rank < cur => {
                best_rank = Some(rank);
                best = vec![(key_parts, *value)];
            }
            Some(cur) if rank == cur => {
                best.push((key_parts, *value));
            }
            _ => {}
        }
    }
    let distinct: HashSet<&Vec<&str>> = best.iter().map(|(parts, _)| parts).collect();
    if distinct.len() != 1 {
        return None;
    }
    best.into_iter().next().map(|(_, v)| v)
}

/// Suffix-ranked lookup matching `mutator.coverage._lookup`.
pub fn lookup_entry<'a, V, F>(
    index: &'a HashMap<String, V>,
    source_path: &str,
    name: Option<&str>,
    normalize: F,
) -> Option<&'a V>
where
    F: Fn(&str) -> String,
{
    if index.is_empty() {
        return None;
    }
    // Preserve first insertion for colliding normalized keys (matching Python dict comprehension?
    // Actually in Python `{normalize(k): v for k, v in index.items()}` keeps last; for tests with
    // trailing slash, let's insert keeping first only if needed? Wait: test_the_first_of_two_equal_paths_keeps_its_hits
    // uses `pre/...` vs `pre/.../` where `normalize_path` leaves trailing slash so keys differ).
    let mut normalized: HashMap<String, &V> = HashMap::with_capacity(index.len());
    for (k, v) in index {
        normalized.insert(normalize(k), v);
    }
    let mut cands = candidates(normalize, source_path);
    if let Some(n) = name
        && !n.is_empty()
    {
        cands.push(n.to_string());
    }
    if let Some(found) = exact(&normalized, &cands) {
        return Some(found);
    }
    let sources: Vec<Vec<&str>> = cands
        .iter()
        .map(|c| path_parts(c))
        .filter(|p| !p.is_empty())
        .collect();
    best_value(&normalized, &sources)
}

fn as_suffix_str(longer: &[String], shorter: &[&str]) -> Option<usize> {
    if longer.len() >= shorter.len()
        && longer[longer.len() - shorter.len()..]
            .iter()
            .zip(shorter.iter())
            .all(|(a, b)| a == b)
    {
        Some(longer.len() - shorter.len())
    } else {
        None
    }
}

fn suffix_rank_str(key_parts: &[String], source: &[&str]) -> Option<(u8, usize)> {
    if source.is_empty() || key_parts.is_empty() {
        return None;
    }
    if let Some(extra) = as_suffix_str(key_parts, source) {
        return Some((0, extra));
    }
    let key_refs: Vec<&str> = key_parts.iter().map(String::as_str).collect();
    as_suffix(source, &key_refs).map(|missing| (1, missing))
}

fn best_rank_for_key_str(key_parts: &[String], sources: &[Vec<&str>]) -> Option<(u8, usize)> {
    sources
        .iter()
        .filter_map(|src| suffix_rank_str(key_parts, src))
        .min()
}

fn best_entry<'a, V>(entries: &'a [(Vec<String>, V)], sources: &[Vec<&str>]) -> Option<&'a V> {
    let mut best_rank: Option<(u8, usize)> = None;
    let mut best: Vec<(&[String], &'a V)> = Vec::new();
    for (key_parts, value) in entries {
        let Some(rank) = best_rank_for_key_str(key_parts, sources) else {
            continue;
        };
        match best_rank {
            None => {
                best_rank = Some(rank);
                best = vec![(key_parts.as_slice(), value)];
            }
            Some(cur) if rank < cur => {
                best_rank = Some(rank);
                best = vec![(key_parts.as_slice(), value)];
            }
            Some(cur) if rank == cur => {
                best.push((key_parts.as_slice(), value));
            }
            _ => {}
        }
    }
    let distinct: HashSet<&[String]> = best.iter().map(|(parts, _)| *parts).collect();
    if distinct.len() != 1 {
        return None;
    }
    best.into_iter().next().map(|(_, v)| v)
}

/// Pre-normalized path index supporting O(1) exact lookups and pre-split suffix ranking.
#[derive(Debug, Clone)]
pub struct NormalizedIndex<V> {
    entries: Vec<(Vec<String>, V)>,
    by_norm: HashMap<String, usize>,
}

impl<V> NormalizedIndex<V> {
    /// Normalize all keys once and build an index matching `lookup_entry` semantics.
    pub fn build<I, F>(items: I, normalize: F) -> Self
    where
        I: IntoIterator<Item = (String, V)>,
        F: Fn(&str) -> String,
    {
        let mut entries: Vec<(Vec<String>, V)> = Vec::new();
        let mut by_norm: HashMap<String, usize> = HashMap::new();
        for (raw_key, value) in items {
            let norm = normalize(&raw_key);
            if let Some(&idx) = by_norm.get(&norm) {
                entries[idx].1 = value;
            } else {
                let parts = norm
                    .split('/')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
                let idx = entries.len();
                entries.push((parts, value));
                by_norm.insert(norm, idx);
            }
        }
        Self { entries, by_norm }
    }

    /// True when the index holds no coverage entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lookup `source_path` (or optional `name` fallback) using exact match first, then suffix rank.
    pub fn lookup<F>(&self, source_path: &str, name: Option<&str>, normalize: F) -> Option<&V>
    where
        F: Fn(&str) -> String,
    {
        if self.entries.is_empty() {
            return None;
        }
        let mut cands = candidates(normalize, source_path);
        if let Some(n) = name
            && !n.is_empty()
        {
            cands.push(n.to_string());
        }
        for cand in &cands {
            if let Some(&idx) = self.by_norm.get(cand) {
                return Some(&self.entries[idx].1);
            }
        }
        let sources: Vec<Vec<&str>> = cands
            .iter()
            .map(|c| path_parts(c))
            .filter(|p| !p.is_empty())
            .collect();
        best_entry(&self.entries, &sources)
    }
}

/// Extract the set of covered 1-based line numbers from `(covered, total)` pairs.
pub fn covered_from_pairs(
    lines: Option<&HashMap<usize, (usize, usize)>>,
) -> Option<HashSet<usize>> {
    let lines = lines?;
    Some(
        lines
            .iter()
            .filter_map(|(&number, &(covered, _))| if covered > 0 { Some(number) } else { None })
            .collect(),
    )
}

fn is_skip_dir_name(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()).is_some_and(|n| {
        matches!(
            n,
            ".git"
                | ".hg"
                | ".svn"
                | ".idea"
                | ".venv"
                | "venv"
                | "node_modules"
                | "vendor"
                | "target"
                | "dist"
                | "build"
                | "out"
                | "coverage"
                | ".metrics"
                | "testdata"
                | "__pycache__"
                | ".clj-kondo"
                | ".build"
                | ".swiftpm"
                | "DerivedData"
        )
    })
}

fn jacoco_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        root.join("target")
            .join("site")
            .join("jacoco")
            .join("jacoco.xml"),
    ];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && !is_skip_dir_name(&p) {
                paths.push(
                    p.join("target")
                        .join("site")
                        .join("jacoco")
                        .join("jacoco.xml"),
                );
                if let Ok(subentries) = std::fs::read_dir(&p) {
                    for sub in subentries.flatten() {
                        let sp = sub.path();
                        if sp.is_dir() && !is_skip_dir_name(&sp) {
                            paths.push(
                                sp.join("target")
                                    .join("site")
                                    .join("jacoco")
                                    .join("jacoco.xml"),
                            );
                        }
                    }
                }
            }
        }
    }
    paths
}

fn parse_jacoco_xml(text: &str) -> HashMap<String, HashSet<usize>> {
    let cleaned = DOCTYPE_RE.replace_all(text, "");
    let Ok(doc) = roxmltree::Document::parse(&cleaned) else {
        return HashMap::new();
    };
    let mut found = HashMap::new();
    for package in doc.descendants().filter(|n| n.has_tag_name("package")) {
        let pkg_name = package.attribute("name").unwrap_or("").trim_matches('/');
        for source in package.children().filter(|n| n.has_tag_name("sourcefile")) {
            let filename = source.attribute("name").unwrap_or("");
            let key = if pkg_name.is_empty() {
                filename.to_string()
            } else {
                format!("{pkg_name}/{filename}")
            };
            let mut covered = HashSet::new();
            for line in source.children().filter(|n| n.has_tag_name("line")) {
                let nr = line.attribute("nr").unwrap_or("0").parse::<usize>();
                let ci = line.attribute("ci").unwrap_or("0").parse::<usize>();
                let cb = line.attribute("cb").unwrap_or("0").parse::<usize>();
                if let (Ok(nr), Ok(ci), Ok(cb)) = (nr, ci, cb)
                    && (ci > 0 || cb > 0)
                {
                    covered.insert(nr);
                }
            }
            found.insert(key, covered);
        }
    }
    found
}

/// Build an index of `package/File.java -> covered line numbers` from JaCoCo XML reports under `root`.
pub fn jacoco_index(root: &Path) -> HashMap<String, HashSet<usize>> {
    let mut found = HashMap::new();
    let mut seen = HashSet::new();
    for path in jacoco_paths(root) {
        if !path.is_file() {
            continue;
        }
        let resolved = resolve_path(&path);
        if !seen.insert(resolved) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        found.extend(parse_jacoco_xml(&text));
    }
    found
}

/// Lines hit in a Swift file from llvm-cov regions or xccov function summaries.
pub fn swift_hits(record: &SwiftCoverageRecord) -> HashSet<usize> {
    if !record.regions.is_empty() {
        let mut covered = HashSet::new();
        for &(start, end, _) in &record.regions {
            for line in start..=end {
                if record.line_covered(line) {
                    covered.insert(line);
                }
            }
        }
        return covered;
    }
    record
        .summaries
        .iter()
        .filter_map(|&(line, covered, _)| if covered > 0 { Some(line) } else { None })
        .collect()
}

/// Lines with coverage hits for `source_path`, or `None` when absent from coverage reports.
pub fn covered_lines_with<C: CoverageProvider>(
    root: &Path,
    source_path: &Path,
    language: &str,
    provider: &C,
) -> Option<HashSet<usize>> {
    let path = to_posix(&resolve_path(source_path));
    if language == "java" {
        let index = jacoco_index(root);
        return lookup_entry(&index, &path, None, |p| provider.normalize_path(p)).cloned();
    }
    if language == "go" {
        let profile = provider.load_go_profile(root)?;
        if profile.is_empty() {
            return None;
        }
        let resolved_root = resolve_path(root);
        let name = provider.go_import_name(Path::new(&path), &resolved_root);
        let segments = lookup_entry(&profile, &path, name.as_deref(), |p| {
            provider.normalize_path(p)
        })?;
        let mut covered = HashSet::new();
        for &(start, end, _statements, hits) in segments {
            if hits == 0 {
                continue;
            }
            for line in start..=end {
                covered.insert(line);
            }
        }
        return Some(covered);
    }
    if language == "swift" {
        let record = provider.lookup_swift(root, &path)?;
        return Some(swift_hits(&record));
    }
    if language == "clojure" {
        let html_idx = provider.load_form_html(root);
        if let Some(html) = covered_from_pairs(lookup_entry(&html_idx, &path, None, |p| {
            provider.normalize_path(p)
        })) {
            return Some(html);
        }
    }
    let lcov_idx = provider.load_lcov(root);
    covered_from_pairs(lookup_entry(&lcov_idx, &path, None, |p| {
        provider.normalize_path(p)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct DummyProvider {
        lcov: HashMap<String, HashMap<usize, (usize, usize)>>,
        go_profile: Option<GoProfileMap>,
        swift: Option<SwiftCoverageRecord>,
    }

    impl CoverageProvider for DummyProvider {
        fn normalize_path(&self, path: &str) -> String {
            let mut text = path.replace('\\', "/");
            if let Some(rest) = text.strip_prefix("file:") {
                text = rest.to_string();
            }
            while let Some(rest) = text.strip_prefix("./") {
                text = rest.to_string();
            }
            while text.contains("//") {
                text = text.replace("//", "/");
            }
            text
        }
        fn go_import_name(&self, _source_path: &Path, _root: &Path) -> Option<String> {
            None
        }
        fn load_go_profile(
            &self,
            _root: &Path,
        ) -> Option<HashMap<String, Vec<(usize, usize, usize, u64)>>> {
            self.go_profile.clone()
        }
        fn load_form_html(&self, _root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>> {
            HashMap::new()
        }
        fn load_lcov(&self, _root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>> {
            self.lcov.clone()
        }
        fn lookup_swift(&self, _root: &Path, _source_path: &str) -> Option<SwiftCoverageRecord> {
            self.swift.clone()
        }
    }

    #[test]
    fn jacoco_and_swift_regions_work() {
        let root = std::env::temp_dir().join(format!("mutator_cov_{}", uuid::Uuid::new_v4()));
        let report = root
            .join("target")
            .join("site")
            .join("jacoco")
            .join("jacoco.xml");
        std::fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::fs::write(
            &report,
            "<?xml version=\"1.0\"?>\n<!DOCTYPE report SYSTEM \"report.dtd\">\n<report name=\"demo\"><package name=\"demo\"><sourcefile name=\"Board.java\"><line nr=\"3\" mi=\"0\" ci=\"2\" mb=\"0\" cb=\"0\"/><line nr=\"4\" mi=\"4\" ci=\"0\" mb=\"1\" cb=\"0\"/><line nr=\"5\" mi=\"0\" ci=\"0\" mb=\"0\" cb=\"1\"/></sourcefile></package></report>",
        )
        .unwrap();
        let src = root.join("src").join("demo").join("Board.java");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "class Board {}\n").unwrap();
        assert_eq!(
            covered_lines_with(&root, &src, "java", &DummyProvider::default()),
            Some(HashSet::from([3, 5]))
        );

        let swift = SwiftCoverageRecord {
            regions: vec![(4, 7, 3), (6, 7, 0)],
            summaries: vec![],
        };
        assert_eq!(swift_hits(&swift), HashSet::from([4, 5]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn suffix_matching_prevents_short_path_stealing() {
        let root = std::env::temp_dir().join(format!("mutator_lcov_{}", uuid::Uuid::new_v4()));
        let short = root.join("b").join("app.py");
        let long = root.join("a").join("b").join("app.py");
        std::fs::create_dir_all(short.parent().unwrap()).unwrap();
        std::fs::create_dir_all(long.parent().unwrap()).unwrap();
        std::fs::write(&short, "def p():\n    return 1\n").unwrap();
        std::fs::write(&long, "def p():\n    return 1\n").unwrap();

        let prov = DummyProvider {
            lcov: HashMap::from([
                ("b/app.py".into(), HashMap::from([(2, (1, 1))])),
                ("a/b/app.py".into(), HashMap::from([(1, (1, 1))])),
            ]),
            ..Default::default()
        };
        assert_eq!(
            covered_lines_with(&root, &long, "python", &prov),
            Some(HashSet::from([1]))
        );
        assert_eq!(
            covered_lines_with(&root, &short, "python", &prov),
            Some(HashSet::from([2]))
        );

        let indexed = NormalizedIndex::build(prov.lcov.clone(), |p| prov.normalize_path(p));
        let long_posix = to_posix(&resolve_path(&long));
        let short_posix = to_posix(&resolve_path(&short));
        assert_eq!(
            indexed.lookup(&long_posix, None, |p| prov.normalize_path(p)),
            Some(&HashMap::from([(1, (1, 1))]))
        );
        assert_eq!(
            indexed.lookup(&short_posix, None, |p| prov.normalize_path(p)),
            Some(&HashMap::from([(2, (1, 1))]))
        );
        let ambiguous = NormalizedIndex::build(
            [
                ("x/shared/a.ts".to_string(), 1usize),
                ("y/shared/a.ts".to_string(), 2usize),
            ],
            |p| prov.normalize_path(p),
        );
        assert_eq!(
            ambiguous.lookup("shared/a.ts", None, |p| prov.normalize_path(p)),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
