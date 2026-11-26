//! Apply each selected mutant, run tests, and record the snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::metrics::{load_history, source_key, write_results};
use crate::model::{FormResult, History, RunResult, Site};
use crate::path_util::{resolve_path, to_posix};
use crate::report::format_scan;
use crate::runner::{Command, Runner, display_command};
use crate::workers::{MutantRunConfig, run_mutants};

/// Cached baseline key `(command_identity, working_directory_string)`.
pub type BaselineKey = (Vec<String>, String);
/// Cached baseline outcome `(passed, duration_seconds, failure_tail)`.
pub type BaselineEntry = (bool, f64, String);
/// Cache of baselines across files in a run.
pub type BaselineCache = HashMap<BaselineKey, BaselineEntry>;

use crate::sources::DiscoverProvider;

/// `(start_line, end_line, name, private)` span tuple for a form.
pub type FormSpan = (usize, usize, String, bool);
/// Map of `(namespace, form_id)` -> `FormSpan`.
pub type FormSpanMap = HashMap<(String, String), FormSpan>;

/// Span and digest provider backed by `crapper` + `sites` once linked.
pub trait EngineProvider: DiscoverProvider {
    fn sites_in_file(&self, source: &str, path: &Path, root: &Path, file_key: &str) -> Vec<Site>;
    fn form_digests(
        &self,
        source: &str,
        path: &Path,
        root: &Path,
    ) -> HashMap<(String, String), String>;
    fn form_spans(&self, source: &str, path: &Path, root: &Path) -> FormSpanMap;
    fn test_plan(
        &self,
        root: &Path,
        source: &Path,
        language: &str,
        override_cmd: Option<&str>,
        xcode_scheme: Option<&str>,
        xcode_destination: Option<&str>,
    ) -> (Command, PathBuf);
}

fn collect_backup_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_backup_files(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// Put back sources left mutated by an interrupted older run.
pub fn restore_backups(root: &Path) -> Vec<PathBuf> {
    let base = root.join("target").join("mutator-backup");
    let mut restored = Vec::new();
    if !base.is_dir() {
        return restored;
    }
    let mut backups = Vec::new();
    collect_backup_files(&base, &mut backups);
    backups.sort();
    for backup in backups {
        let Ok(relative) = backup.strip_prefix(&base) else {
            continue;
        };
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = std::fs::read(&backup)
            && std::fs::write(&target, bytes).is_ok()
        {
            let _ = std::fs::remove_file(&backup);
            restored.push(target);
        }
    }
    restored
}

/// Remove same-second `.pyc` cache files for `path` so Python sees mutated sources.
pub fn drop_bytecode(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let cache = parent.join("__pycache__");
    if !cache.is_dir() {
        return;
    }
    let prefix = format!("{stem}.");
    if let Ok(entries) = std::fs::read_dir(cache) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with(&prefix) && s.ends_with(".pyc") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Save original bytes into `target/mutator-backup` if not already present.
pub fn backup(root: &Path, path: &Path, original: &[u8]) -> std::io::Result<PathBuf> {
    let resolved_path = resolve_path(path);
    let resolved_root = resolve_path(root);
    let relative = resolved_path
        .strip_prefix(&resolved_root)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let backup_path = root.join("target").join("mutator-backup").join(relative);
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !backup_path.exists() {
        std::fs::write(&backup_path, original)?;
    }
    Ok(backup_path)
}

pub fn is_covered(site: &Site, lines: Option<&HashSet<usize>>, ignore_coverage: bool) -> bool {
    if ignore_coverage {
        return true;
    }
    let Some(lines) = lines else {
        return false;
    };
    lines.contains(&site.line) || lines.contains(&site.anchor_line)
}

pub fn coverage_map(
    found: &[Site],
    covered_lines: Option<&HashSet<usize>>,
    ignore_coverage: bool,
) -> HashMap<String, bool> {
    found
        .iter()
        .map(|site| {
            (
                site.mutation_id.clone(),
                is_covered(site, covered_lines, ignore_coverage),
            )
        })
        .collect()
}

pub fn select_sites(
    sites: &[Site],
    covered: &HashMap<String, bool>,
    history: &History,
    digests: &HashMap<(String, String), String>,
    mutate_all: bool,
    lines: Option<&HashSet<usize>>,
) -> Vec<Site> {
    let mut chosen = Vec::new();
    for site in sites {
        if !covered.get(&site.mutation_id).copied().unwrap_or(false) {
            continue;
        }
        if let Some(line_filter) = lines
            && !line_filter.contains(&site.line)
        {
            continue;
        }
        let key = (site.namespace.clone(), site.form_id.clone());
        let unchanged = history
            .forms
            .get(&key)
            .is_some_and(|prior| digests.get(&key) == Some(&prior.digest));
        if mutate_all || !unchanged {
            chosen.push(site.clone());
            continue;
        }
        if history.outcomes.get(&site.mutation_id).map(String::as_str) == Some("killed") {
            continue;
        }
        chosen.push(site.clone());
    }
    chosen
}

pub fn carry_forward(
    sites: &[Site],
    covered: &HashMap<String, bool>,
    history: &History,
    digests: &HashMap<(String, String), String>,
) -> HashMap<String, String> {
    let by_id: HashMap<&str, &Site> = sites.iter().map(|s| (s.mutation_id.as_str(), s)).collect();
    let mut carried = HashMap::new();
    for (mutation, status) in &history.outcomes {
        let Some(site) = by_id.get(mutation.as_str()) else {
            continue;
        };
        if !covered.get(&site.mutation_id).copied().unwrap_or(false) {
            continue;
        }
        let key = (site.namespace.clone(), site.form_id.clone());
        if history
            .forms
            .get(&key)
            .is_some_and(|prior| digests.get(&key) == Some(&prior.digest))
        {
            carried.insert(mutation.clone(), status.clone());
        }
    }
    carried
}

/// Inputs for summarizing per-form mutation outcomes.
pub struct FormSummaryInput<'a> {
    pub source: &'a str,
    pub path: &'a Path,
    pub root: &'a Path,
    pub file_key: &'a str,
    pub sites: &'a [Site],
    pub covered: &'a HashMap<String, bool>,
    pub outcomes: &'a HashMap<String, String>,
    pub lines: Option<&'a HashSet<usize>>,
}

pub fn build_forms<E: EngineProvider>(
    input: &FormSummaryInput<'_>,
    provider: &E,
) -> Vec<FormResult> {
    let spans = provider.form_spans(input.source, input.path, input.root);
    let digests = provider.form_digests(input.source, input.path, input.root);
    let mut grouped: HashMap<(String, String), Vec<&Site>> = HashMap::new();
    for site in input.sites {
        grouped
            .entry((site.namespace.clone(), site.form_id.clone()))
            .or_default()
            .push(site);
    }

    let mut ordered: Vec<(&(String, String), &FormSpan)> = spans.iter().collect();
    ordered.sort_by_key(|((_, _), (start, _, _, _))| *start);

    let mut forms = Vec::with_capacity(ordered.len());
    for (key, (start, end, name, private)) in ordered {
        let (namespace, form) = key;
        let owned = grouped.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let mut killed = 0usize;
        let mut survived = 0usize;
        let mut uncovered = 0usize;
        for site in owned {
            if !input
                .covered
                .get(&site.mutation_id)
                .copied()
                .unwrap_or(false)
            {
                uncovered += 1;
                continue;
            }
            match input.outcomes.get(&site.mutation_id).map(String::as_str) {
                Some("killed") => killed += 1,
                Some("survived") => survived += 1,
                _ if input.lines.is_none() => survived += 1,
                _ => {}
            }
        }
        forms.push(FormResult {
            id: form.clone(),
            namespace: namespace.clone(),
            name: name.clone(),
            private: *private,
            file: input.file_key.to_string(),
            line: *start,
            end_line: *end,
            digest: digests.get(key).cloned().unwrap_or_default(),
            killed,
            survived,
            uncovered,
            sites: owned.len(),
        });
    }
    forms
}

fn decode_utf8_file(path: &Path) -> Option<(Vec<u8>, String)> {
    let original = std::fs::read(path).ok()?;
    let source = String::from_utf8(original.clone()).ok()?;
    Some((original, source))
}

fn warn_uncovered(
    file_key: &str,
    found: &[Site],
    covered_lines: Option<&HashSet<usize>>,
    ignore_coverage: bool,
) {
    if covered_lines.is_some() || ignore_coverage || found.is_empty() {
        return;
    }
    eprintln!(
        "No coverage data for {file_key}; its sites are uncovered. Pass --no-coverage to run them anyway."
    );
}

fn warn_many(file_key: &str, selected: &[Site], mutation_warning: i64) {
    if (selected.len() as i64) > mutation_warning {
        eprintln!(
            "WARNING: {} covered mutations selected in {file_key}.",
            selected.len()
        );
    }
}

fn command_key(command: &Command) -> Vec<String> {
    match command {
        Command::Shell(s) => vec!["sh".to_string(), s.clone()],
        Command::Argv(args) => {
            let mut key = Vec::with_capacity(args.len() + 1);
            key.push("argv".to_string());
            key.extend(args.iter().cloned());
            key
        }
    }
}

fn last_lines(output: &str, count: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

fn remember_baseline<R: Runner>(
    baselines: &mut BaselineCache,
    cache_key: BaselineKey,
    runner: &R,
    command: &Command,
    cwd: &Path,
) {
    if baselines.contains_key(&cache_key) {
        return;
    }
    let baseline = runner.run(command, cwd, None);
    if baseline.code != 0 {
        let tail = last_lines(&baseline.output, 20);
        baselines.insert(cache_key, (false, baseline.seconds, tail));
    } else {
        baselines.insert(cache_key, (true, baseline.seconds, String::new()));
    }
}

fn baseline_failure(file_key: &str, command: &Command, tail: &str) -> RunResult {
    let mut message = format!(
        "Baseline failed for {file_key}: {}",
        display_command(command)
    );
    if !tail.is_empty() {
        message.push('\n');
        message.push_str(tail);
    }
    RunResult {
        path: file_key.to_string(),
        baseline_failed: true,
        baseline_message: message,
        ..Default::default()
    }
}

fn statuses(
    found: &[Site],
    covered: &HashMap<String, bool>,
    outcomes: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for site in found {
        if !covered.get(&site.mutation_id).copied().unwrap_or(false) {
            out.insert(site.mutation_id.clone(), "uncovered".to_string());
        } else if let Some(status) = outcomes.get(&site.mutation_id) {
            out.insert(site.mutation_id.clone(), status.clone());
        }
    }
    out
}

/// Restore `path` if its on-disk bytes differ from `original`.
pub fn restore_if_dirty(path: &Path, original: &[u8]) {
    if std::fs::read(path).ok().as_deref() != Some(original) {
        let _ = std::fs::write(path, original);
    }
}

/// Options for running mutation testing on a single file.
pub struct MutateOptions<'a> {
    pub covered_lines: Option<&'a HashSet<usize>>,
    pub ignore_coverage: bool,
    pub mutate_all: bool,
    pub lines: Option<&'a HashSet<usize>>,
    pub test_command: Option<&'a str>,
    pub timeout_factor: f64,
    pub mutation_warning: i64,
    pub max_workers: Option<usize>,
    pub xcode_scheme: Option<&'a str>,
    pub xcode_destination: Option<&'a str>,
}

/// Mutate one file with a preloaded prior snapshot `history`.
#[allow(clippy::too_many_arguments)]
pub fn mutate_file_with_history<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    history: &History,
    provider: &E,
) -> RunResult {
    mutate_file_impl(path, root, runner, opts, baselines, Some(history), provider)
}

/// Mutate one file and write its namespaces into `.metrics/mutate`.
pub fn mutate_file_with<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    provider: &E,
) -> RunResult {
    mutate_file_impl(path, root, runner, opts, baselines, None, provider)
}

fn mutate_file_impl<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    preloaded_history: Option<&History>,
    provider: &E,
) -> RunResult {
    let root = resolve_path(root);
    let path = resolve_path(path);
    let Ok(file_key) = source_key(&path, &root) else {
        return RunResult {
            path: to_posix(&path),
            skipped: "outside project root".to_string(),
            ..Default::default()
        };
    };
    let Some(language) = provider.language_of(&path) else {
        return RunResult {
            path: file_key,
            skipped: "unsupported file".to_string(),
            ..Default::default()
        };
    };
    let Some((original, source)) = decode_utf8_file(&path) else {
        return RunResult {
            path: file_key,
            skipped: "file is not UTF-8".to_string(),
            ..Default::default()
        };
    };

    let found = provider.sites_in_file(&source, &path, &root, &file_key);
    let covered = coverage_map(&found, opts.covered_lines, opts.ignore_coverage);
    warn_uncovered(&file_key, &found, opts.covered_lines, opts.ignore_coverage);

    let owned_history;
    let history = match preloaded_history {
        Some(h) => h,
        None => {
            owned_history = load_history(&root, &file_key);
            &owned_history
        }
    };
    let digests = provider.form_digests(&source, &path, &root);
    let selected = select_sites(
        &found,
        &covered,
        history,
        &digests,
        opts.mutate_all,
        opts.lines,
    );
    let mut outcomes = carry_forward(&found, &covered, history, &digests);
    warn_many(&file_key, &selected, opts.mutation_warning);

    if !selected.is_empty() {
        let (command, cwd) = provider.test_plan(
            &root,
            &path,
            language,
            opts.test_command,
            opts.xcode_scheme,
            opts.xcode_destination,
        );
        let cache_key = (command_key(&command), cwd.to_string_lossy().into_owned());
        remember_baseline(baselines, cache_key.clone(), runner, &command, &cwd);
        let (ok, seconds, tail) =
            baselines
                .get(&cache_key)
                .cloned()
                .unwrap_or((false, 0.0, String::new()));
        if !ok {
            return baseline_failure(&file_key, &command, &tail);
        }
        let timeout = 2.0_f64.max(seconds * opts.timeout_factor);
        let run_cfg = MutantRunConfig {
            root: &root,
            original: &original,
            command: &command,
            cwd: &cwd,
            timeout,
            file_key: &file_key,
        };
        if let Err(err) = run_mutants(
            &path,
            &selected,
            opts.max_workers,
            runner,
            &run_cfg,
            &mut outcomes,
        ) {
            return RunResult {
                path: file_key,
                baseline_failed: true,
                baseline_message: format!("Worker overlay error: {err}"),
                ..Default::default()
            };
        }
    }

    let summary_input = FormSummaryInput {
        source: &source,
        path: &path,
        root: &root,
        file_key: &file_key,
        sites: &found,
        covered: &covered,
        outcomes: &outcomes,
        lines: opts.lines,
    };
    let forms = build_forms(&summary_input, provider);
    let written = match write_results(&root, &file_key, &forms, &outcomes) {
        Ok(paths) => paths.into_iter().map(|p| to_posix(&p)).collect(),
        Err(err) => {
            return RunResult {
                path: file_key,
                baseline_failed: true,
                baseline_message: format!("Failed to write metrics: {err}"),
                ..Default::default()
            };
        }
    };
    restore_if_dirty(&path, &original);

    RunResult {
        path: file_key,
        forms,
        written,
        statuses: statuses(&found, &covered, &outcomes),
        sites: found,
        ..Default::default()
    }
}

/// Inventory mutation sites with a preloaded prior snapshot `history`.
pub fn scan_file_with_history<E: EngineProvider>(
    path: &Path,
    root: &Path,
    covered_lines: Option<&HashSet<usize>>,
    ignore_coverage: bool,
    lines: Option<&HashSet<usize>>,
    history: &History,
    provider: &E,
) -> Result<String, String> {
    scan_file_impl(
        path,
        root,
        covered_lines,
        ignore_coverage,
        lines,
        Some(history),
        provider,
    )
}

/// Inventory mutation sites without running tests or writing metrics.
pub fn scan_file_with<E: EngineProvider>(
    path: &Path,
    root: &Path,
    covered_lines: Option<&HashSet<usize>>,
    ignore_coverage: bool,
    lines: Option<&HashSet<usize>>,
    provider: &E,
) -> Result<String, String> {
    scan_file_impl(
        path,
        root,
        covered_lines,
        ignore_coverage,
        lines,
        None,
        provider,
    )
}

fn scan_file_impl<E: EngineProvider>(
    path: &Path,
    root: &Path,
    covered_lines: Option<&HashSet<usize>>,
    ignore_coverage: bool,
    lines: Option<&HashSet<usize>>,
    preloaded_history: Option<&History>,
    provider: &E,
) -> Result<String, String> {
    let root = resolve_path(root);
    let path = resolve_path(path);
    let file_key = source_key(&path, &root)?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let source = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut found = provider.sites_in_file(&source, &path, &root, &file_key);
    if let Some(line_filter) = lines {
        found.retain(|s| line_filter.contains(&s.line));
    }
    let covered = if !ignore_coverage && covered_lines.is_some() {
        Some(coverage_map(&found, covered_lines, ignore_coverage))
    } else {
        None
    };
    let owned_history;
    let history = match preloaded_history {
        Some(h) => h,
        None => {
            owned_history = load_history(&root, &file_key);
            &owned_history
        }
    };
    let mut changed = HashSet::new();
    if !history.forms.is_empty() {
        let digests = provider.form_digests(&source, &path, &root);
        for (key, digest) in &digests {
            let same = history
                .forms
                .get(key)
                .is_some_and(|prior| &prior.digest == digest);
            if !same {
                changed.insert(key.clone());
            }
        }
    }
    Ok(format_scan(&file_key, &found, &changed, covered.as_ref()))
}

/// Ordered keys helper used by tests and diagnostics.
pub fn sorted_outcome_keys(outcomes: &HashMap<String, String>) -> BTreeMap<&str, &str> {
    outcomes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_backups_and_bytecode_cleanup() {
        let root = std::env::temp_dir().join(format!("mutator_eng_{}", uuid::Uuid::new_v4()));
        let backup_file = root
            .join("target")
            .join("mutator-backup")
            .join("a")
            .join("b")
            .join("demo.py");
        std::fs::create_dir_all(backup_file.parent().unwrap()).unwrap();
        std::fs::write(&backup_file, b"original\n").unwrap();
        let restored = restore_backups(&root);
        assert_eq!(restored, vec![root.join("a").join("b").join("demo.py")]);
        assert_eq!(
            std::fs::read(root.join("a").join("b").join("demo.py")).unwrap(),
            b"original\n"
        );

        let src = root.join("src").join("demo.py");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"one").unwrap();
        let saved = backup(&root, &src, b"one").unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"one");
        assert_eq!(
            std::fs::read(backup(&root, &src, b"two").unwrap()).unwrap(),
            b"one"
        );

        let cache = src.parent().unwrap().join("__pycache__");
        std::fs::create_dir_all(&cache).unwrap();
        let pyc = cache.join("demo.cpython-314.pyc");
        std::fs::write(&pyc, b"pyc").unwrap();
        drop_bytecode(&src);
        assert!(!pyc.exists());

        std::fs::write(&src, b"dirty").unwrap();
        restore_if_dirty(&src, b"clean");
        assert_eq!(std::fs::read(&src).unwrap(), b"clean");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_skips_form_digests_without_prior_history() {
        use std::cell::Cell;
        struct CountingProvider(Cell<usize>);
        impl DiscoverProvider for CountingProvider {
            fn language_of(&self, _path: &Path) -> Option<&'static str> {
                Some("python")
            }
            fn is_test_filename(&self, _name: &str) -> bool {
                false
            }
            fn skip_dirs(&self) -> &[&str] {
                &[]
            }
            fn test_dirs(&self) -> &[&str] {
                &[]
            }
        }
        impl EngineProvider for CountingProvider {
            fn sites_in_file(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
                _file_key: &str,
            ) -> Vec<Site> {
                Vec::new()
            }
            fn form_digests(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
            ) -> HashMap<(String, String), String> {
                self.0.set(self.0.get() + 1);
                HashMap::new()
            }
            fn form_spans(&self, _source: &str, _path: &Path, _root: &Path) -> FormSpanMap {
                HashMap::new()
            }
            fn test_plan(
                &self,
                _root: &Path,
                _source: &Path,
                _language: &str,
                _override_cmd: Option<&str>,
                _xcode_scheme: Option<&str>,
                _xcode_destination: Option<&str>,
            ) -> (Command, PathBuf) {
                (Command::Argv(vec!["true".into()]), PathBuf::from("."))
            }
        }

        let root = std::env::temp_dir().join(format!("mutator_scan_skip_{}", uuid::Uuid::new_v4()));
        let file = root.join("src").join("demo.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x = 1\n").unwrap();
        let prov = CountingProvider(Cell::new(0));
        let out =
            scan_file_with_history(&file, &root, None, true, None, &History::default(), &prov)
                .unwrap();
        assert!(out.contains("0 mutation sites"));
        assert_eq!(prov.0.get(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
