//! Run mutants in symlink overlays, preserving binary bytes and cleaning up reliably.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

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
    Regex::new(r#"(?:from|import|require)\s*\(?\s*['"](\.[^'"]+)['"]"#).expect("valid regex")
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

/// Create a symlink from `link` to resolved `target`, replacing any existing link/file at `link`.
pub fn symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(link).is_ok() {
        remove_link_entry(link)?;
    }
    let resolved = resolve_path(target);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&resolved, link)
    }
    #[cfg(windows)]
    {
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&resolved, link)
        } else {
            std::os::windows::fs::symlink_file(&resolved, link)
        }
    }
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
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            delete_once(&entry.path())?;
        }
        return std::fs::remove_dir(path);
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn copy_configs(worker: &Path, root: &Path, relative: &str) -> std::io::Result<()> {
    for &name in CONFIGS {
        let source = root.join(name);
        if !source.is_file() || name == relative {
            continue;
        }
        let bytes = std::fs::read(&source)?;
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
        symlink(&destination, &child)?;
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

fn strip_js_extension(base: &Path) -> PathBuf {
    let text = base.to_string_lossy();
    for ext in JS_EXTENSIONS {
        if let Some(stem) = text.strip_suffix(ext) {
            return PathBuf::from(stem);
        }
    }
    base.to_path_buf()
}

fn candidates(directory: &Path, specifier: &str) -> Vec<PathBuf> {
    let base = directory.join(specifier);
    let stem = strip_js_extension(&base);
    let stem_text = stem.to_string_lossy();
    let mut names = vec![base];
    for ext in JS_EXTENSIONS {
        names.push(PathBuf::from(format!("{stem_text}{ext}")));
    }
    for ext in JS_EXTENSIONS {
        names.push(stem.join(format!("index{ext}")));
    }
    names
}

fn inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let resolved_root = resolve_path(root);
    let resolved_path = resolve_path(path);
    let rel = resolved_path.strip_prefix(&resolved_root).ok()?;
    Some(root.join(rel))
}

fn imported_files(importer: &Path, root: &Path) -> HashSet<PathBuf> {
    let Ok(text) = std::fs::read_to_string(importer) else {
        return HashSet::new();
    };
    let parent = importer.parent().unwrap_or(root);
    let mut found = HashSet::new();
    for spec in specifiers(&text) {
        for cand in candidates(parent, &spec) {
            if let Some(local) = inside(root, &cand)
                && local.is_file()
            {
                found.insert(resolve_path(&local));
            }
        }
    }
    found
}

fn collect_js_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if !is_skip_link(&name_str) && name_str != "node_modules" {
                collect_js_sources(&path, out);
            }
        } else if has_js_extension(&path) {
            out.push(path);
        }
    }
}

/// Relative POSIX paths of JS/TS modules that reach `relative` via relative imports.
pub fn importers_of(root: &Path, relative: &str) -> HashSet<String> {
    let target = resolve_path(&root.join(relative));
    let mut sources = Vec::new();
    collect_js_sources(root, &mut sources);

    let mut reverse: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for source in sources {
        let source_resolved = resolve_path(&source);
        for imported in imported_files(&source, root) {
            reverse
                .entry(imported)
                .or_default()
                .insert(source_resolved.clone());
        }
    }

    let mut found: HashSet<PathBuf> = HashSet::new();
    let mut pending = vec![target.clone()];
    while let Some(current) = pending.pop() {
        if let Some(importers) = reverse.get(&current) {
            for importer in importers {
                if found.contains(importer) || importer == &target {
                    continue;
                }
                found.insert(importer.clone());
                pending.push(importer.clone());
            }
        }
    }

    let root_resolved = resolve_path(root);
    found
        .into_iter()
        .filter_map(|p| p.strip_prefix(&root_resolved).ok().map(to_posix))
        .collect()
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
        symlink(&link.join(name), &entry.path())?;
    }
    Ok(())
}

fn copy_importer(worker: &Path, root: &Path, relative: &str) -> std::io::Result<()> {
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
    let bytes = std::fs::read(root.join(relative))?;
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

/// Create `count` parallel worker overlay directories under `base`.
pub fn create_workers(
    base: &Path,
    root: &Path,
    relative: &str,
    original: &[u8],
    count: usize,
) -> std::io::Result<Vec<PathBuf>> {
    let mut created = Vec::with_capacity(count);
    let first = relative.split('/').next().unwrap_or(relative);
    let importers = importers_of(root, relative);
    for index in 0..count {
        let worker = base.join(format!("worker-{index}"));
        std::fs::create_dir_all(&worker)?;
        copy_configs(&worker, root, relative)?;
        let mut top_skip = HashSet::new();
        top_skip.insert(first);
        link_children(&worker, root, &top_skip)?;
        overlay(&worker, root, relative, original)?;
        for importer in &importers {
            copy_importer(&worker, root, importer)?;
        }
        created.push(worker);
    }
    Ok(created)
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

/// Mutate `sites` in parallel overlays of `root`, storing statuses in `outcomes`.
pub fn run_mutants<R: Runner + Sync>(
    source: &Path,
    sites: &[Site],
    max_workers: Option<usize>,
    runner: &R,
    cfg: &MutantRunConfig<'_>,
    outcomes: &mut HashMap<String, String>,
) -> std::io::Result<()> {
    if sites.is_empty() {
        return Ok(());
    }
    let base = new_run_dir(cfg.root)?;
    let mut cleanup = RunDirCleanup(Some(&base));

    let relative = relative_posix(source, cfg.root)
        .ok_or_else(|| std::io::Error::other("source is outside project root"))?;
    let count = worker_count(sites.len(), max_workers);
    let directories = create_workers(&base, cfg.root, &relative, cfg.original, count)?;

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
