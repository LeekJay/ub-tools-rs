//! Run mutants in symlink overlays, preserving binary bytes and cleaning up reliably.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use rayon::prelude::*;
use regex::Regex;

use crate::model::Site;
use crate::path_util::{relative_posix, resolve_path, to_posix};
use crate::runner::{Command, Runner};

/// Directories excluded from worker symlink overlays.
pub const SKIP_LINK: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".venv",
    "venv",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    ".metrics",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".clj-kondo",
    ".gradle",
    ".build",
    ".swiftpm",
    "DerivedData",
];

/// Top-level manifest/config files copied privately into each worker.
pub const CONFIGS: &[&str] = &[
    "deps.edn",
    "bb.edn",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "go.mod",
    "go.sum",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "tsconfig.json",
    "Cargo.toml",
    "Cargo.lock",
    "pyproject.toml",
    "pytest.ini",
    "setup.cfg",
    "setup.py",
    "conftest.py",
    "requirements.txt",
    "Pipfile",
    "poetry.lock",
];

const JS_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

static SPECIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:from|import|require)\s*\(?\s*['"](\.[^'"]*)['"]"#).expect("valid regex")
});

/// Replace UTF-8 byte slice `start..end` in `source` with `mutant`.
pub fn apply_site(source: &str, start: usize, end: usize, mutant: &str) -> Result<String, String> {
    let data = source.as_bytes();
    if start > end || end > data.len() {
        return Err(format!(
            "byte span {start}..{end} out of range {}",
            data.len()
        ));
    }
    let mut updated = Vec::with_capacity(data.len() - (end - start) + mutant.len());
    updated.extend_from_slice(&data[..start]);
    updated.extend_from_slice(mutant.as_bytes());
    updated.extend_from_slice(&data[end..]);
    String::from_utf8(updated).map_err(|e| e.to_string())
}

/// Apply a byte mutation directly on raw file bytes so non-mutated lines (including CRLF) are preserved verbatim.
pub fn apply_site_bytes(
    original: &[u8],
    start: usize,
    end: usize,
    mutant: &str,
) -> Result<Vec<u8>, String> {
    if start > end || end > original.len() {
        return Err(format!(
            "byte span {start}..{end} out of range {}",
            original.len()
        ));
    }
    let mut updated = Vec::with_capacity(original.len() - (end - start) + mutant.len());
    updated.extend_from_slice(&original[..start]);
    updated.extend_from_slice(mutant.as_bytes());
    updated.extend_from_slice(&original[end..]);
    Ok(updated)
}

/// Smaller of site count, available cores, and optional `--max-workers` (or default worker cap), clamped to at least 1.
pub fn worker_count(site_count: usize, requested: Option<usize>) -> usize {
    let processors = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    worker_count_for(site_count, processors, requested)
}

fn worker_count_for(site_count: usize, processors: usize, requested: Option<usize>) -> usize {
    let limit = match requested {
        Some(n) => n,
        None => ub_runtime::default_mutator_workers(processors),
    };
    1.max(site_count.min(processors).min(limit))
}

/// Create a fresh run directory under `root/target/mutation-workers`.
pub fn new_run_dir(root: &Path) -> std::io::Result<PathBuf> {
    let directory = root
        .join("target")
        .join("mutation-workers")
        .join(format!("run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

fn remove_link_entry(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
            let res = if (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0 {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            return match res {
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        #[cfg(not(windows))]
        {
            return match std::fs::remove_file(path) {
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
    }
    if meta.is_dir() {
        std::fs::remove_dir(path)
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

fn create_os_symlink(link: &Path, resolved_target: &Path, is_dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let _ = is_dir;
        std::os::unix::fs::symlink(resolved_target, link)
    }
    #[cfg(windows)]
    {
        if is_dir {
            std::os::windows::fs::symlink_dir(resolved_target, link)
        } else {
            std::os::windows::fs::symlink_file(resolved_target, link)
        }
    }
}

fn symlink_known(
    link: &Path,
    resolved_target: &Path,
    is_dir: bool,
    check_existing: bool,
) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if check_existing && std::fs::symlink_metadata(link).is_ok() {
        remove_link_entry(link)?;
    }
    create_os_symlink(link, resolved_target, is_dir)
}

/// Create a symlink from `link` to resolved `target`, replacing any existing link/file at `link`.
pub fn symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    let resolved = resolve_path(target);
    symlink_known(link, &resolved, resolved.is_dir(), true)
}

/// Remove a worker tree without following symlinks into the real project.
pub fn delete_tree(path: &Path) -> std::io::Result<()> {
    delete_tree_with(path, 20, std::thread::sleep)
}

/// Retryable tree deletion with configurable attempt count and sleep hook for tests.
pub fn delete_tree_with<F>(path: &Path, attempts: usize, mut sleep: F) -> std::io::Result<()>
where
    F: FnMut(Duration),
{
    let mut last_err = None;
    for attempt in 0..attempts {
        match delete_once(path) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    sleep(Duration::from_secs_f64(0.25 * (attempt + 1) as f64));
                }
            }
        }
    }
    match last_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn delete_once_seq(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return remove_link_entry(path);
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            delete_once_seq(&entry.path())?;
        }
        return std::fs::remove_dir(path);
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn delete_once(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return remove_link_entry(path);
    }
    if meta.is_dir() {
        let entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .map(|entry| entry.map(|item| item.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        if entries.len() > 1 {
            entries.par_iter().try_for_each(|p| delete_once_seq(p))?;
        } else {
            for child in &entries {
                delete_once_seq(child)?;
            }
        }
        return std::fs::remove_dir(path);
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn read_configs(root: &Path, relative: &str) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut configs = Vec::new();
    for &name in CONFIGS {
        let source = root.join(name);
        if !source.is_file() || name == relative {
            continue;
        }
        configs.push((name.to_string(), std::fs::read(&source)?));
    }
    Ok(configs)
}

fn write_configs(worker: &Path, configs: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    for (name, bytes) in configs {
        std::fs::write(worker.join(name), bytes)?;
    }
    Ok(())
}

fn is_xcode_bundle(path: &Path) -> bool {
    let ext = path.extension().and_then(|s| s.to_str());
    matches!(ext, Some("xcodeproj" | "xcworkspace")) && path.is_dir()
}

fn copy_bundle(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let child = entry.path();
        let target = destination.join(entry.file_name());
        if child.is_dir() {
            copy_bundle(&child, &target)?;
        } else {
            let bytes = std::fs::read(&child)?;
            std::fs::write(target, bytes)?;
        }
    }
    Ok(())
}

fn is_skip_link(name: &str) -> bool {
    SKIP_LINK.contains(&name)
}

fn link_children(worker_dir: &Path, real_dir: &Path, skip: &HashSet<&str>) -> std::io::Result<()> {
    if !real_dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(worker_dir)?;
    for entry in std::fs::read_dir(real_dir)? {
        let entry = entry?;
        let child_name = entry.file_name();
        let name_str = child_name.to_string_lossy();
        if skip.contains(name_str.as_ref()) || is_skip_link(name_str.as_ref()) {
            continue;
        }
        let child = entry.path();
        let destination = worker_dir.join(&child_name);
        if destination.exists() || std::fs::symlink_metadata(&destination).is_ok() {
            continue;
        }
        if is_xcode_bundle(&child) {
            copy_bundle(&child, &destination)?;
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            symlink(&destination, &child)?;
        } else {
            create_os_symlink(&destination, &child, ft.is_dir())?;
        }
    }
    Ok(())
}

fn specifiers(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for cap in SPECIFIER_RE.captures_iter(text) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let stripped = raw.split('?').next().unwrap_or(raw);
        let stripped = stripped.split('#').next().unwrap_or(stripped);
        if stripped.starts_with('.') {
            found.push(stripped.to_string());
        }
    }
    found
}

fn has_js_extension(path: &Path) -> bool {
    let text = path.to_string_lossy();
    JS_EXTENSIONS.iter().any(|ext| text.ends_with(ext))
}

fn strip_js_extension_str(path: &str) -> &str {
    for ext in JS_EXTENSIONS {
        if let Some(stem) = path.strip_suffix(ext) {
            return stem;
        }
    }
    path
}

fn lookup_key(rel: &str) -> String {
    #[cfg(windows)]
    {
        rel.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        rel.to_string()
    }
}

fn resolve_relative_posix(parent_rel: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if parent_rel.is_empty() {
        Vec::new()
    } else {
        parent_rel.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in spec.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn collect_js_sources_fast(root_resolved: &Path) -> Vec<(PathBuf, String)> {
    fn push_if_under(path: PathBuf, root_resolved: &Path, out: &mut Vec<(PathBuf, String)>) {
        if let Some(rel) = path.strip_prefix(root_resolved).ok().map(to_posix) {
            out.push((path, rel));
        }
    }
    fn walk(dir: &Path, root_resolved: &Path, in_symlink: bool, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if ft.is_dir() {
                if !is_skip_link(&name_str) && name_str != "node_modules" {
                    walk(&path, root_resolved, in_symlink, out);
                }
            } else if ft.is_symlink() {
                if path.is_dir() {
                    if !is_skip_link(&name_str) && name_str != "node_modules" {
                        walk(&path, root_resolved, true, out);
                    }
                } else if path.is_file() && has_js_extension(&path) {
                    push_if_under(resolve_path(&path), root_resolved, out);
                }
            } else if ft.is_file() && has_js_extension(&path) {
                let target = if in_symlink {
                    resolve_path(&path)
                } else {
                    path
                };
                push_if_under(target, root_resolved, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root_resolved, root_resolved, false, &mut out);
    out
}

fn imported_rel_paths(
    source_rel: &str,
    text: &str,
    known: &HashMap<String, String>,
) -> HashSet<String> {
    if !text.contains("import") && !text.contains("from") && !text.contains("require") {
        return HashSet::new();
    }
    let parent_rel = match source_rel.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    };
    let mut found = HashSet::new();
    for spec in specifiers(text) {
        let Some(norm) = resolve_relative_posix(parent_rel, &spec) else {
            continue;
        };
        if !norm.is_empty()
            && let Some(canonical) = known.get(&lookup_key(&norm))
        {
            found.insert(canonical.clone());
        }
        let stem = strip_js_extension_str(&norm);
        if !stem.is_empty() {
            for ext in JS_EXTENSIONS {
                let cand = format!("{stem}{ext}");
                if let Some(canonical) = known.get(&lookup_key(&cand)) {
                    found.insert(canonical.clone());
                }
            }
        }
        for ext in JS_EXTENSIONS {
            let cand = if stem.is_empty() {
                format!("index{ext}")
            } else {
                format!("{stem}/index{ext}")
            };
            if let Some(canonical) = known.get(&lookup_key(&cand)) {
                found.insert(canonical.clone());
            }
        }
    }
    found
}

/// Reusable in-memory reverse dependency graph for JS/TS relative imports.
#[derive(Debug, Default, Clone)]
pub struct ImportGraph {
    canonical: HashMap<String, String>,
    reverse: HashMap<String, Vec<String>>,
}

impl ImportGraph {
    /// Build a reverse-import graph across all JS/TS modules under `root` in parallel.
    pub fn build(root: &Path) -> Self {
        let root_resolved = resolve_path(root);
        let sources = collect_js_sources_fast(&root_resolved);
        let mut canonical = HashMap::with_capacity(sources.len());
        for (_, rel) in &sources {
            canonical.insert(lookup_key(rel), rel.clone());
        }

        let edges: Vec<(String, HashSet<String>)> = sources
            .par_iter()
            .filter_map(|(path, rel)| {
                let text = std::fs::read_to_string(path).ok()?;
                let imported = imported_rel_paths(rel, &text, &canonical);
                if imported.is_empty() {
                    None
                } else {
                    Some((rel.clone(), imported))
                }
            })
            .collect();

        let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
        for (importer, targets) in edges {
            for target in targets {
                reverse.entry(target).or_default().push(importer.clone());
            }
        }
        Self { canonical, reverse }
    }

    /// Return relative POSIX paths of all JS/TS modules that transitively import `relative`.
    pub fn importers_of(&self, relative: &str) -> HashSet<String> {
        let normalized = relative.replace('\\', "/");
        let trimmed = normalized.strip_prefix("./").unwrap_or(&normalized);
        if !has_js_extension(Path::new(trimmed)) {
            return HashSet::new();
        }
        let target = self
            .canonical
            .get(&lookup_key(trimmed))
            .map(String::as_str)
            .unwrap_or(trimmed);

        let mut found: HashSet<String> = HashSet::new();
        let mut pending: Vec<&str> = vec![target];
        while let Some(current) = pending.pop() {
            let Some(importers) = self.reverse.get(current) else {
                continue;
            };
            for importer in importers {
                if importer == target || found.contains(importer.as_str()) {
                    continue;
                }
                found.insert(importer.clone());
                pending.push(importer.as_str());
            }
        }
        found
    }
}

/// Per-run worker state caching the repository's `ImportGraph` across mutated files.
#[derive(Debug, Default)]
pub struct WorkerContext {
    cached: Option<(PathBuf, ImportGraph)>,
}

impl WorkerContext {
    /// Create an empty worker context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up transitive JS/TS importers for `relative` under `root`, caching the graph per root.
    pub fn importers_of(&mut self, root: &Path, relative: &str) -> HashSet<String> {
        if !has_js_extension(Path::new(relative)) {
            return HashSet::new();
        }
        let root_resolved = if self.cached.as_ref().is_some_and(|(r, _)| r == root) {
            root.to_path_buf()
        } else {
            resolve_path(root)
        };
        let needs_build = !self
            .cached
            .as_ref()
            .is_some_and(|(r, _)| r == &root_resolved);
        if needs_build {
            let graph = ImportGraph::build(&root_resolved);
            self.cached = Some((root_resolved, graph));
        }
        self.cached
            .as_ref()
            .map(|(_, graph)| graph.importers_of(relative))
            .unwrap_or_default()
    }
}

/// Relative POSIX paths of JS/TS modules that reach `relative` via relative imports.
pub fn importers_of(root: &Path, relative: &str) -> HashSet<String> {
    if !has_js_extension(Path::new(relative)) {
        return HashSet::new();
    }
    ImportGraph::build(root).importers_of(relative)
}

fn expand_directory(link: &Path, real_dir: &Path) -> std::io::Result<()> {
    if !real_dir.is_dir() {
        return Ok(());
    }
    remove_link_entry(link)?;
    std::fs::create_dir_all(link)?;
    for entry in std::fs::read_dir(real_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_skip_link(&name.to_string_lossy()) {
            continue;
        }
        let child = entry.path();
        let dest = link.join(name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            symlink(&dest, &child)?;
        } else {
            create_os_symlink(&dest, &child, ft.is_dir())?;
        }
    }
    Ok(())
}

fn write_importer(worker: &Path, relative: &str, bytes: &[u8], root: &Path) -> std::io::Result<()> {
    let mut real = root.to_path_buf();
    let mut current = worker.to_path_buf();
    let segments: Vec<&str> = relative.split('/').collect();
    for &segment in &segments[..segments.len().saturating_sub(1)] {
        real = real.join(segment);
        current = current.join(segment);
        let is_sym = std::fs::symlink_metadata(&current)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_sym {
            expand_directory(&current, &real)?;
        } else if !current.exists() {
            std::fs::create_dir_all(&current)?;
        }
    }

    let destination = worker.join(relative);
    let dest_is_sym = std::fs::symlink_metadata(&destination)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if dest_is_sym {
        remove_link_entry(&destination)?;
    } else if destination.exists() {
        return Ok(());
    }
    std::fs::write(destination, bytes)
}

fn overlay(worker: &Path, root: &Path, relative: &str, original: &[u8]) -> std::io::Result<()> {
    let segments: Vec<&str> = relative.split('/').collect();
    for index in 0..segments.len().saturating_sub(1) {
        let mut rel_dir = PathBuf::new();
        for seg in &segments[..=index] {
            rel_dir.push(seg);
        }
        let mut skip = HashSet::new();
        skip.insert(segments[index + 1]);
        link_children(&worker.join(&rel_dir), &root.join(&rel_dir), &skip)?;
    }
    let mut destination = worker.to_path_buf();
    for seg in &segments {
        destination.push(seg);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(destination, original)
}

fn create_workers_with_importers(
    base: &Path,
    root_resolved: &Path,
    relative: &str,
    original: &[u8],
    count: usize,
    importers: &HashSet<String>,
) -> std::io::Result<Vec<PathBuf>> {
    let first = relative.split('/').next().unwrap_or(relative);
    let configs = read_configs(root_resolved, relative)?;
    let mut sorted_importers: Vec<&str> = importers.iter().map(String::as_str).collect();
    sorted_importers.sort_unstable();
    let importer_bytes: Vec<(&str, Vec<u8>)> = sorted_importers
        .par_iter()
        .map(|&rel| std::fs::read(root_resolved.join(rel)).map(|bytes| (rel, bytes)))
        .collect::<std::io::Result<Vec<_>>>()?;

    (0..count)
        .into_par_iter()
        .map(|index| {
            let worker = base.join(format!("worker-{index}"));
            std::fs::create_dir_all(&worker)?;
            write_configs(&worker, &configs)?;
            let mut top_skip = HashSet::new();
            top_skip.insert(first);
            link_children(&worker, root_resolved, &top_skip)?;
            overlay(&worker, root_resolved, relative, original)?;
            for (importer, bytes) in &importer_bytes {
                write_importer(&worker, importer, bytes, root_resolved)?;
            }
            Ok(worker)
        })
        .collect()
}

/// Create `count` parallel worker overlay directories under `base`.
pub fn create_workers(
    base: &Path,
    root: &Path,
    relative: &str,
    original: &[u8],
    count: usize,
) -> std::io::Result<Vec<PathBuf>> {
    let root_resolved = resolve_path(root);
    let importers = importers_of(&root_resolved, relative);
    create_workers_with_importers(base, &root_resolved, relative, original, count, &importers)
}

/// Map a real-tree `cwd` into the equivalent path inside `worker`.
pub fn mapped_cwd(worker: &Path, root: &Path, cwd: &Path) -> PathBuf {
    let resolved_root = resolve_path(root);
    let resolved_cwd = resolve_path(cwd);
    match resolved_cwd.strip_prefix(&resolved_root) {
        Ok(rel) => resolve_path(&worker.join(rel)),
        Err(_) => worker.to_path_buf(),
    }
}

struct RestoreGuard<'a> {
    destination: &'a Path,
    original: &'a [u8],
}

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(self.destination, self.original);
    }
}

/// Shared configuration for executing mutants across worker overlays.
pub struct MutantRunConfig<'a> {
    pub root: &'a Path,
    pub original: &'a [u8],
    pub command: &'a Command,
    pub cwd: &'a Path,
    pub timeout: f64,
    pub file_key: &'a str,
}

pub fn execute_site<R: Runner>(
    directory: &Path,
    site: &Site,
    relative: &str,
    runner: &R,
    cfg: &MutantRunConfig<'_>,
) -> String {
    let slice_ok = cfg
        .original
        .get(site.start..site.end)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        == Some(site.original.as_str());
    if !slice_ok {
        eprintln!(
            "Skipped {}:{line} {desc}; source bytes moved",
            cfg.file_key,
            line = site.line,
            desc = site.description()
        );
        return "survived".to_string();
    }

    let destination = directory.join(relative);
    let Ok(mutated) = apply_site_bytes(cfg.original, site.start, site.end, &site.mutant) else {
        return "survived".to_string();
    };
    if std::fs::write(&destination, &mutated).is_err() {
        return "survived".to_string();
    }
    let _guard = RestoreGuard {
        destination: &destination,
        original: cfg.original,
    };
    let work_dir = mapped_cwd(directory, cfg.root, cfg.cwd);
    let result = runner.run(cfg.command, &work_dir, Some(cfg.timeout));
    if result.timed_out || result.code != 0 {
        "killed".to_string()
    } else {
        "survived".to_string()
    }
}

fn prune_empty_run_parents(base: &Path) {
    if let Some(workers_dir) = base.parent()
        && std::fs::remove_dir(workers_dir).is_ok()
        && let Some(target_dir) = workers_dir.parent()
    {
        let _ = std::fs::remove_dir(target_dir);
    }
}

struct RunDirCleanup<'a>(Option<&'a Path>);

impl Drop for RunDirCleanup<'_> {
    fn drop(&mut self) {
        if let Some(base) = self.0 {
            let _ = delete_tree(base);
            prune_empty_run_parents(base);
        }
    }
}

/// Mutate `sites` in parallel overlays of `root`, reusing `ctx`'s cached import graph across files.
pub(crate) fn run_mutants_with_context<R: Runner + Sync>(
    source: &Path,
    sites: &[Site],
    max_workers: Option<usize>,
    runner: &R,
    cfg: &MutantRunConfig<'_>,
    outcomes: &mut HashMap<String, String>,
    ctx: &mut WorkerContext,
) -> std::io::Result<()> {
    if sites.is_empty() {
        return Ok(());
    }
    let root_resolved = resolve_path(cfg.root);
    let base = new_run_dir(&root_resolved)?;
    let mut cleanup = RunDirCleanup(Some(&base));

    let relative = relative_posix(source, &root_resolved)
        .ok_or_else(|| std::io::Error::other("source is outside project root"))?;
    let importers = ctx.importers_of(&root_resolved, &relative);
    let count = worker_count(sites.len(), max_workers);
    let directories = create_workers_with_importers(
        &base,
        &root_resolved,
        &relative,
        cfg.original,
        count,
        &importers,
    )?;

    let queue: Mutex<VecDeque<&Site>> = Mutex::new(sites.iter().collect());
    let results: Mutex<Vec<(String, String)>> = Mutex::new(Vec::with_capacity(sites.len()));

    std::thread::scope(|scope| {
        for directory in &directories {
            let queue_ref = &queue;
            let results_ref = &results;
            let rel_ref = relative.as_str();
            scope.spawn(move || {
                loop {
                    let next_site = {
                        let mut guard = queue_ref.lock().expect("queue lock");
                        guard.pop_front()
                    };
                    let Some(site) = next_site else {
                        break;
                    };
                    if runner.is_verbose() {
                        eprintln!("{}:{} {}", cfg.file_key, site.line, site.description());
                    }
                    let status = execute_site(directory, site, rel_ref, runner, cfg);
                    let mut out_guard = results_ref.lock().expect("results lock");
                    out_guard.push((site.mutation_id.clone(), status));
                }
            });
        }
    });

    for (id, status) in results.into_inner().expect("results unwrap") {
        outcomes.insert(id, status);
    }
    cleanup.0 = None;
    delete_tree(&base)?;
    prune_empty_run_parents(&base);
    Ok(())
}

/// Mutate `sites` in parallel overlays of `root`, storing statuses in `outcomes`.
pub fn run_mutants<R: Runner + Sync>(
    source: &Path,
    sites: &[Site],
    max_workers: Option<usize>,
    runner: &R,
    cfg: &MutantRunConfig<'_>,
    outcomes: &mut HashMap<String, String>,
) -> std::io::Result<()> {
    let mut ctx = WorkerContext::new();
    run_mutants_with_context(source, sites, max_workers, runner, cfg, outcomes, &mut ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandResult;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mutator_workers_test_{name}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct FakeRunner(std::sync::Arc<Mutex<Vec<Vec<u8>>>>, PathBuf);

    impl Runner for FakeRunner {
        fn is_verbose(&self) -> bool {
            false
        }
        fn run(&self, _command: &Command, _cwd: &Path, _timeout: Option<f64>) -> CommandResult {
            let observed = std::fs::read(&self.1).unwrap();
            self.0.lock().unwrap().push(observed);
            CommandResult {
                code: 1,
                timed_out: false,
                seconds: 0.01,
                output: String::new(),
            }
        }
    }

    #[test]
    fn execute_preserves_crlf_line_endings() {
        let tmp = temp_dir("crlf");
        let root = tmp.join("root");
        let worker = tmp.join("worker");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(worker.join("src")).unwrap();
        let original = b"def f():\r\n    return 1 + 2\r\n";
        let worker_file = worker.join("src").join("demo.py");
        std::fs::write(&worker_file, original).unwrap();
        let offset = original.iter().position(|&b| b == b'+').unwrap();
        let site = Site {
            file: "src/demo.py".into(),
            namespace: "demo".into(),
            form_id: "defn/f".into(),
            name: "f".into(),
            line: 2,
            start: offset,
            end: offset + 1,
            original: "+".into(),
            mutant: "-".into(),
            category: "arithmetic".into(),
            mutation_id: "m1".into(),
            anchor_line: 2,
        };
        let observed = std::sync::Arc::new(Mutex::new(Vec::new()));
        let runner = FakeRunner(observed.clone(), worker_file.clone());
        let cmd = Command::Argv(vec!["pytest".into()]);
        let cfg = MutantRunConfig {
            root: &root,
            original,
            command: &cmd,
            cwd: &root,
            timeout: 5.0,
            file_key: "src/demo.py",
        };
        let status = execute_site(&worker, &site, "src/demo.py", &runner, &cfg);
        assert_eq!(status, "killed");
        let seen = observed.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[b"def f():\r\n    return 1 - 2\r\n".to_vec()]
        );
        assert_eq!(std::fs::read(&worker_file).unwrap(), original);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_tree_unlinks_symlink_without_following() {
        let tmp = temp_dir("sym");
        let real = tmp.join("project");
        std::fs::create_dir_all(&real).unwrap();
        let kept = real.join("important.txt");
        std::fs::write(&kept, "keep").unwrap();

        let tree = tmp.join("worker");
        std::fs::create_dir_all(&tree).unwrap();
        symlink(&tree.join("src"), &real).unwrap();
        let link_root = tmp.join("worker-link");
        symlink(&link_root, &real).unwrap();

        delete_tree(&tree).unwrap();
        delete_tree(&link_root).unwrap();
        assert!(!tree.exists());
        assert!(!link_root.exists());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn importers_and_workers_create_private_copies() {
        let root = temp_dir("importers");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let find = root.join("src").join("find.mjs");
        std::fs::write(&find, "export const value = \"real\";\n").unwrap();
        std::fs::write(
            root.join("src").join("find.test.mjs"),
            "import { value } from \"./find.mjs\";\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("notes.mjs"),
            "export const notes = 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests").join("use.mjs"),
            "import { value } from \"../src/find.mjs\";\n",
        )
        .unwrap();
        let base = new_run_dir(&root).unwrap();
        let workers = create_workers(
            &base,
            &root,
            "src/find.mjs",
            &std::fs::read(&find).unwrap(),
            1,
        )
        .unwrap();
        let w = &workers[0];
        assert!(w.join("src").join("find.mjs").is_file());
        assert!(
            !std::fs::symlink_metadata(w.join("src").join("find.mjs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !std::fs::symlink_metadata(w.join("src").join("find.test.mjs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !std::fs::symlink_metadata(w.join("tests").join("use.mjs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::symlink_metadata(w.join("src").join("notes.mjs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        delete_tree(&base).unwrap();
        assert!(!base.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_graph_and_worker_context_handle_transitive_esm_index_escaping_and_reuse() {
        let root = temp_dir("import_graph");
        std::fs::create_dir_all(root.join("src").join("sub")).unwrap();
        std::fs::create_dir_all(root.join("outside_sibling")).unwrap();
        std::fs::write(root.join("index.ts"), "export const rootIdx = 1;\n").unwrap();
        std::fs::write(root.join("src").join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(
            root.join("src").join("b.ts"),
            "import { a } from './a.js';\nexport const b = a + 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("sub").join("index.ts"),
            "import { b } from '../b';\nexport const sub = b;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("sub").join("leaf.ts"),
            "import { sub } from '.';\nimport { rootIdx } from '../..';\nimport { gone } from '../../../outside.ts';\nexport const leaf = [sub, rootIdx, gone];\n",
        )
        .unwrap();
        std::fs::write(root.join("src").join("app.py"), "def f(x):\n    return x\n").unwrap();

        let graph = ImportGraph::build(&root);
        let a_importers = graph.importers_of("src/a.ts");
        assert_eq!(
            a_importers,
            HashSet::from([
                "src/b.ts".to_string(),
                "src/sub/index.ts".to_string(),
                "src/sub/leaf.ts".to_string(),
            ])
        );
        assert_eq!(
            graph.importers_of("index.ts"),
            HashSet::from(["src/sub/leaf.ts".to_string()])
        );
        assert!(graph.importers_of("src/app.py").is_empty());

        let mut ctx = WorkerContext::new();
        // Non-JS target stays lazy (no graph built).
        assert!(ctx.importers_of(&root, "src/app.py").is_empty());
        assert!(ctx.cached.is_none());

        // First JS target builds and caches graph under resolved root.
        assert_eq!(ctx.importers_of(&root, "src/a.ts"), a_importers);
        assert!(ctx.cached.is_some());

        // Delete a.ts-unrelated edge on disk? Reuse proves cached graph returns prior state without rescanning.
        std::fs::remove_file(root.join("src").join("sub").join("leaf.ts")).unwrap();
        assert_eq!(ctx.importers_of(&root, "src/a.ts"), a_importers);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parallel_create_workers_creates_four_isolated_overlays_and_cleans_up() {
        let root = temp_dir("parallel_workers");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), "{\"name\":\"demo\"}\n").unwrap();
        let target = root.join("src").join("calc.ts");
        std::fs::write(&target, "export const v = 1;\n").unwrap();
        std::fs::write(
            root.join("src").join("calc.test.ts"),
            "import { v } from './calc';\nexport const t = v;\n",
        )
        .unwrap();
        std::fs::write(root.join("src").join("other.ts"), "export const o = 2;\n").unwrap();

        let base = new_run_dir(&root).unwrap();
        let original = std::fs::read(&target).unwrap();
        let workers = create_workers(&base, &root, "src/calc.ts", &original, 4).unwrap();
        assert_eq!(workers.len(), 4);
        for (idx, w) in workers.iter().enumerate() {
            assert!(w.ends_with(format!("worker-{idx}")));
            assert!(w.join("package.json").is_file());
            assert!(
                !std::fs::symlink_metadata(w.join("src").join("calc.ts"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(
                !std::fs::symlink_metadata(w.join("src").join("calc.test.ts"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(
                std::fs::symlink_metadata(w.join("src").join("other.ts"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        delete_tree(&base).unwrap();
        assert!(!base.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_count_caps_default_and_preserves_explicit_limit() {
        let processors = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let default_limit = ub_runtime::default_mutator_workers(processors);
        assert_eq!(
            worker_count(1000, None),
            1.max(processors.min(default_limit))
        );
        assert_eq!(worker_count(1, None), 1);
        assert_eq!(worker_count(0, None), 1);
        assert_eq!(worker_count(1000, Some(processors)), processors);
        assert_eq!(worker_count(1000, Some(processors + 8)), processors);
        assert_eq!(worker_count(1000, Some(2)), 1.max(processors.min(2)));
        assert_eq!(worker_count(1000, Some(0)), 1);
    }

    #[test]
    fn worker_count_for_caps_16_cores_and_honors_env_override_in_subprocess() {
        const SUBPROC_ENV: &str = "MUTATOR_WORKERS_TEST_SUBPROC";
        const MODE_ENV: &str = "MUTATOR_WORKERS_TEST_MODE";

        if std::env::var_os(SUBPROC_ENV).is_some() {
            match std::env::var(MODE_ENV).as_deref() {
                Ok("default") => {
                    assert_eq!(worker_count_for(100, 16, None), 4);
                    assert_eq!(worker_count_for(2, 16, None), 2);
                    assert_eq!(worker_count_for(0, 16, None), 1);
                    assert_eq!(worker_count_for(100, 16, Some(12)), 12);
                    assert_eq!(worker_count_for(100, 16, Some(32)), 16);
                }
                Ok("override") => {
                    assert_eq!(worker_count_for(100, 16, None), 6);
                    assert_eq!(worker_count_for(100, 16, Some(2)), 2);
                    assert_eq!(worker_count_for(100, 16, Some(12)), 12);
                }
                other => panic!("unexpected mode: {other:?}"),
            }
            return;
        }

        let exe = std::env::current_exe().expect("current test exe");
        for (mode, workers_env) in [("default", None), ("override", Some("6"))] {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args([
                "--exact",
                "workers::tests::worker_count_for_caps_16_cores_and_honors_env_override_in_subprocess",
                "--nocapture",
            ])
            .env(SUBPROC_ENV, "1")
            .env(MODE_ENV, mode);
            match workers_env {
                Some(v) => {
                    cmd.env("UB_TOOLS_MUTATOR_WORKERS", v);
                }
                None => {
                    cmd.env_remove("UB_TOOLS_MUTATOR_WORKERS");
                }
            }
            let out = cmd.output().expect("spawn worker_count subprocess");
            assert!(
                out.status.success(),
                "subprocess mode {mode} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
