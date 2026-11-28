//! Apply each selected mutant, run tests, and record the snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::metrics::{load_history, source_key, write_results};
use crate::model::{FormResult, History, RunResult, ScanFileData, Site};
use crate::path_util::{resolve_path, to_posix};
use crate::report::format_scan;
use crate::runner::{Command, Runner, display_command};
use crate::workers::{MutantRunConfig, WorkerContext, run_mutants, run_mutants_with_context};

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
        "[mutator] WARN: no coverage data for {file_key} ({} sites marked uncovered; generate coverage or pass --no-coverage).",
        found.len()
    );
}

fn warn_many(file_key: &str, selected: &[Site], mutation_warning: i64) {
    if (selected.len() as i64) > mutation_warning {
        eprintln!(
            "[mutator] WARN: {} mutations selected in {file_key} (exceeds warning limit {mutation_warning}; use --lines to batch decision lines).",
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
    pub no_metrics: bool,
}

/// Options for scanning mutation sites in a single file.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanOptions<'a> {
    pub covered_lines: Option<&'a HashSet<usize>>,
    pub ignore_coverage: bool,
    pub lines: Option<&'a HashSet<usize>>,
    pub cached_sites: Option<&'a [Site]>,
}

/// Mutate one file with a preloaded prior snapshot `history` and optional pre-discovered `cached_sites`.
#[allow(clippy::too_many_arguments)]
pub fn mutate_file_cached_with_history<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    history: &History,
    cached_sites: Option<&[Site]>,
    provider: &E,
) -> RunResult {
    mutate_file_impl(
        path,
        root,
        runner,
        opts,
        baselines,
        Some(history),
        cached_sites,
        None,
        provider,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn mutate_file_cached_with_context<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    history: &History,
    cached_sites: Option<&[Site]>,
    ctx: &mut WorkerContext,
    provider: &E,
) -> RunResult {
    mutate_file_impl(
        path,
        root,
        runner,
        opts,
        baselines,
        Some(history),
        cached_sites,
        Some(ctx),
        provider,
    )
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
    mutate_file_impl(
        path,
        root,
        runner,
        opts,
        baselines,
        Some(history),
        None,
        None,
        provider,
    )
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
    mutate_file_impl(
        path, root, runner, opts, baselines, None, None, None, provider,
    )
}

#[allow(clippy::too_many_arguments)]
fn mutate_file_impl<R: Runner + Sync, E: EngineProvider>(
    path: &Path,
    root: &Path,
    runner: &R,
    opts: &MutateOptions<'_>,
    baselines: &mut BaselineCache,
    preloaded_history: Option<&History>,
    cached_sites: Option<&[Site]>,
    worker_ctx: Option<&mut WorkerContext>,
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

    let found = match cached_sites {
        Some(sites) => sites.to_vec(),
        None => provider.sites_in_file(&source, &path, &root, &file_key),
    };
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
        let run_result = match worker_ctx {
            Some(ctx) => run_mutants_with_context(
                &path,
                &selected,
                opts.max_workers,
                runner,
                &run_cfg,
                &mut outcomes,
                ctx,
                Some(seconds),
            ),
            None => run_mutants(
                &path,
                &selected,
                opts.max_workers,
                runner,
                &run_cfg,
                &mut outcomes,
            ),
        };
        if let Err(err) = run_result {
            return RunResult {
                path: file_key,
                baseline_failed: true,
                baseline_message: format!("Worker overlay error: {err}"),
                ..Default::default()
            };
        }
    }

    let mut executed_killed = 0usize;
    let mut executed_survived = 0usize;
    for site in &selected {
        match outcomes.get(&site.mutation_id).map(String::as_str) {
            Some("killed") => executed_killed += 1,
            Some("survived") => executed_survived += 1,
            _ => {}
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
    let written = if opts.no_metrics {
        Vec::new()
    } else {
        match write_results(&root, &file_key, &forms, &outcomes) {
            Ok(paths) => paths.into_iter().map(|p| to_posix(&p)).collect(),
            Err(err) => {
                return RunResult {
                    path: file_key,
                    baseline_failed: true,
                    baseline_message: format!("Failed to write metrics: {err}"),
                    ..Default::default()
                };
            }
        }
    };
    restore_if_dirty(&path, &original);

    RunResult {
        path: file_key,
        forms,
        written,
        statuses: statuses(&found, &covered, &outcomes),
        executed_killed,
        executed_survived,
        sites: found,
        ..Default::default()
    }
}

fn read_utf8_source(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Inventory mutation sites returning structured `ScanFileData` and using `opts.cached_sites` when valid.
///
/// On a cache hit with empty prior form history, avoids reading source bytes from disk.
pub fn scan_file_cached_with_history<E: EngineProvider>(
    path: &Path,
    root: &Path,
    opts: &ScanOptions<'_>,
    history: &History,
    provider: &E,
) -> Result<ScanFileData, String> {
    scan_file_core(
        path,
        root,
        opts,
        Some(history),
        true,
        provider,
        read_utf8_source,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_file_resolved_with_history<E: EngineProvider>(
    resolved_path: &Path,
    resolved_root: &Path,
    file_key: &str,
    opts: &ScanOptions<'_>,
    history: &History,
    provider: &E,
) -> Result<ScanFileData, String> {
    scan_file_resolved_core(
        resolved_path,
        resolved_root,
        file_key.to_string(),
        opts,
        Some(history),
        true,
        provider,
        read_utf8_source,
    )
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
    let opts = ScanOptions {
        covered_lines,
        ignore_coverage,
        lines,
        cached_sites: None,
    };
    let data = scan_file_core(
        path,
        root,
        &opts,
        Some(history),
        false,
        provider,
        read_utf8_source,
    )?;
    Ok(format_scan(
        &data.file,
        &data.sites,
        &data.changed,
        data.covered.as_ref(),
    ))
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
    let opts = ScanOptions {
        covered_lines,
        ignore_coverage,
        lines,
        cached_sites: None,
    };
    let data = scan_file_core(path, root, &opts, None, false, provider, read_utf8_source)?;
    Ok(format_scan(
        &data.file,
        &data.sites,
        &data.changed,
        data.covered.as_ref(),
    ))
}

fn scan_file_core<E: EngineProvider, F>(
    path: &Path,
    root: &Path,
    opts: &ScanOptions<'_>,
    preloaded_history: Option<&History>,
    track_discovered: bool,
    provider: &E,
    read_source: F,
) -> Result<ScanFileData, String>
where
    F: Fn(&Path) -> Result<String, String>,
{
    let root = resolve_path(root);
    let path = resolve_path(path);
    let file_key = source_key(&path, &root)?;
    scan_file_resolved_core(
        &path,
        &root,
        file_key,
        opts,
        preloaded_history,
        track_discovered,
        provider,
        read_source,
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_file_resolved_core<E: EngineProvider, F>(
    path: &Path,
    root: &Path,
    file_key: String,
    opts: &ScanOptions<'_>,
    preloaded_history: Option<&History>,
    track_discovered: bool,
    provider: &E,
    read_source: F,
) -> Result<ScanFileData, String>
where
    F: Fn(&Path) -> Result<String, String>,
{
    let owned_history;
    let history = match preloaded_history {
        Some(h) => h,
        None => {
            owned_history = load_history(root, &file_key);
            &owned_history
        }
    };

    let mut source_opt: Option<String> = None;
    let (mut found, discovered_sites) = match opts.cached_sites {
        Some(cached) => (cached.to_vec(), None),
        None => {
            let source = read_source(path)?;
            let unfiltered = provider.sites_in_file(&source, path, root, &file_key);
            let discovered = if track_discovered {
                Some(unfiltered.clone())
            } else {
                None
            };
            source_opt = Some(source);
            (unfiltered, discovered)
        }
    };

    if let Some(line_filter) = opts.lines {
        found.retain(|s| line_filter.contains(&s.line));
    }

    let covered = if !opts.ignore_coverage {
        Some(coverage_map(&found, opts.covered_lines, false))
    } else {
        None
    };

    let mut changed = HashSet::new();
    let skip_changed_for_lines = opts.lines.is_some() && found.is_empty();
    if !history.forms.is_empty() && !skip_changed_for_lines {
        let source = match source_opt {
            Some(s) => s,
            None => read_source(path)?,
        };
        let digests = provider.form_digests(&source, path, root);
        let active_forms: Option<HashSet<(&str, &str)>> = opts.lines.map(|_| {
            found
                .iter()
                .map(|s| (s.namespace.as_str(), s.form_id.as_str()))
                .collect()
        });
        for (key, digest) in &digests {
            if let Some(ref active) = active_forms
                && !active.contains(&(key.0.as_str(), key.1.as_str()))
            {
                continue;
            }
            let same = history
                .forms
                .get(key)
                .is_some_and(|prior| &prior.digest == digest);
            if !same {
                changed.insert(key.clone());
            }
        }
    }

    Ok(ScanFileData {
        file: file_key,
        sites: found,
        changed,
        covered,
        discovered_sites,
    })
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

    #[test]
    fn scan_cached_skips_read_and_sites_when_history_empty_and_filters_lines() {
        use crate::model::PriorForm;
        use crate::runner::Runner;
        use std::cell::Cell;

        struct SpyProvider {
            sites_calls: Cell<usize>,
            digest_calls: Cell<usize>,
            span_calls: Cell<usize>,
        }
        impl DiscoverProvider for SpyProvider {
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
        impl EngineProvider for SpyProvider {
            fn sites_in_file(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
                file_key: &str,
            ) -> Vec<Site> {
                self.sites_calls.set(self.sites_calls.get() + 1);
                vec![
                    Site {
                        file: file_key.to_string(),
                        namespace: "demo".to_string(),
                        form_id: "defn/f".to_string(),
                        name: "f".to_string(),
                        line: 2,
                        start: 10,
                        end: 11,
                        original: ">".to_string(),
                        mutant: ">=".to_string(),
                        category: "comparison".to_string(),
                        mutation_id: "m1".to_string(),
                        anchor_line: 2,
                    },
                    Site {
                        file: file_key.to_string(),
                        namespace: "demo".to_string(),
                        form_id: "defn/f".to_string(),
                        name: "f".to_string(),
                        line: 3,
                        start: 20,
                        end: 21,
                        original: "+".to_string(),
                        mutant: "-".to_string(),
                        category: "arithmetic".to_string(),
                        mutation_id: "m2".to_string(),
                        anchor_line: 3,
                    },
                ]
            }
            fn form_digests(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
            ) -> HashMap<(String, String), String> {
                self.digest_calls.set(self.digest_calls.get() + 1);
                HashMap::from([(("demo".into(), "defn/f".into()), "new_hash".into())])
            }
            fn form_spans(&self, _source: &str, _path: &Path, _root: &Path) -> FormSpanMap {
                self.span_calls.set(self.span_calls.get() + 1);
                HashMap::from([(("demo".into(), "defn/f".into()), (1, 3, "f".into(), false))])
            }
            fn test_plan(
                &self,
                root: &Path,
                _source: &Path,
                _language: &str,
                _override_cmd: Option<&str>,
                _xcode_scheme: Option<&str>,
                _xcode_destination: Option<&str>,
            ) -> (Command, PathBuf) {
                (Command::Argv(vec!["true".into()]), root.to_path_buf())
            }
        }
        struct NoopRunner;
        impl Runner for NoopRunner {
            fn is_verbose(&self) -> bool {
                false
            }
            fn run(
                &self,
                _command: &Command,
                _cwd: &Path,
                _timeout: Option<f64>,
            ) -> crate::runner::CommandResult {
                crate::runner::CommandResult {
                    code: 0,
                    output: String::new(),
                    timed_out: false,
                    seconds: 0.01,
                }
            }
        }

        let root =
            std::env::temp_dir().join(format!("mutator_cached_eng_{}", uuid::Uuid::new_v4()));
        let file = root.join("src").join("demo.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "def f(x):\n    if x > 0:\n        return x + 1\n").unwrap();

        let prov = SpyProvider {
            sites_calls: Cell::new(0),
            digest_calls: Cell::new(0),
            span_calls: Cell::new(0),
        };
        let empty_hist = History::default();

        // 1. Cache miss: reads source once, runs sites_in_file once, returns unfiltered discovered_sites.
        let read_calls = Cell::new(0usize);
        let line_filter = HashSet::from([2usize]);
        let miss = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: true,
                lines: Some(&line_filter),
                cached_sites: None,
            },
            Some(&empty_hist),
            true,
            &prov,
            |p| {
                read_calls.set(read_calls.get() + 1);
                read_utf8_source(p)
            },
        )
        .unwrap();
        assert_eq!(read_calls.get(), 1);
        assert_eq!(prov.sites_calls.get(), 1);
        assert_eq!(prov.digest_calls.get(), 0);
        assert_eq!(miss.sites.len(), 1);
        assert_eq!(miss.sites[0].line, 2);
        let cached = miss.discovered_sites.unwrap();
        assert_eq!(cached.len(), 2);

        // 2. Cache hit + empty history: skips BOTH fs::read and sites_in_file, still applies line filter!
        let hit_no_hist = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: true,
                lines: Some(&line_filter),
                cached_sites: Some(&cached),
            },
            Some(&empty_hist),
            true,
            &prov,
            |_| panic!("fs::read must be skipped on cache hit with empty history"),
        )
        .unwrap();
        assert_eq!(prov.sites_calls.get(), 1);
        assert_eq!(prov.digest_calls.get(), 0);
        assert!(hit_no_hist.discovered_sites.is_none());
        assert_eq!(hit_no_hist.sites, miss.sites);

        // 3. Cache hit + non-empty history: skips sites_in_file, reads source ONLY for form_digests.
        let mut prior = History::default();
        prior.forms.insert(
            ("demo".into(), "defn/f".into()),
            PriorForm {
                namespace: "demo".into(),
                id: "defn/f".into(),
                digest: "old_hash".into(),
                file: "src/demo.py".into(),
            },
        );
        let read_for_hist = Cell::new(0usize);
        let hit_with_hist = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: true,
                lines: None,
                cached_sites: Some(&cached),
            },
            Some(&prior),
            true,
            &prov,
            |p| {
                read_for_hist.set(read_for_hist.get() + 1);
                read_utf8_source(p)
            },
        )
        .unwrap();
        assert_eq!(read_for_hist.get(), 1);
        assert_eq!(prov.sites_calls.get(), 1);
        assert_eq!(prov.digest_calls.get(), 1);
        assert_eq!(
            hit_with_hist.changed,
            HashSet::from([("demo".to_string(), "defn/f".to_string())])
        );
        assert_eq!(hit_with_hist.sites.len(), 2);

        // 4. Mutate cached variant skips sites_in_file when cached_sites is provided.
        let mut baselines = BaselineCache::new();
        let mutate_opts = MutateOptions {
            covered_lines: None,
            ignore_coverage: false,
            mutate_all: false,
            lines: None,
            test_command: None,
            timeout_factor: 10.0,
            mutation_warning: 50,
            max_workers: Some(1),
            xcode_scheme: None,
            xcode_destination: None,
            no_metrics: false,
        };
        let res = mutate_file_cached_with_history(
            &file,
            &root,
            &NoopRunner,
            &mutate_opts,
            &mut baselines,
            &empty_hist,
            Some(&cached),
            &prov,
        );
        assert_eq!(prov.sites_calls.get(), 1);
        assert_eq!(res.sites.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_marks_missing_coverage_uncovered_and_scopes_changed_forms_to_filtered_lines() {
        use crate::model::PriorForm;
        use std::cell::Cell;

        struct TwoFormProvider {
            digest_calls: Cell<usize>,
        }
        impl DiscoverProvider for TwoFormProvider {
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
        impl EngineProvider for TwoFormProvider {
            fn sites_in_file(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
                file_key: &str,
            ) -> Vec<Site> {
                vec![
                    Site {
                        file: file_key.to_string(),
                        namespace: "demo".to_string(),
                        form_id: "defn/f".to_string(),
                        name: "f".to_string(),
                        line: 2,
                        start: 10,
                        end: 11,
                        original: "+".to_string(),
                        mutant: "-".to_string(),
                        category: "arithmetic".to_string(),
                        mutation_id: "m1".to_string(),
                        anchor_line: 2,
                    },
                    Site {
                        file: file_key.to_string(),
                        namespace: "demo".to_string(),
                        form_id: "defn/g".to_string(),
                        name: "g".to_string(),
                        line: 10,
                        start: 40,
                        end: 41,
                        original: ">".to_string(),
                        mutant: ">=".to_string(),
                        category: "comparison".to_string(),
                        mutation_id: "m2".to_string(),
                        anchor_line: 10,
                    },
                ]
            }
            fn form_digests(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
            ) -> HashMap<(String, String), String> {
                self.digest_calls.set(self.digest_calls.get() + 1);
                HashMap::from([
                    (("demo".into(), "defn/f".into()), "new_f".into()),
                    (("demo".into(), "defn/g".into()), "new_g".into()),
                ])
            }
            fn form_spans(&self, _source: &str, _path: &Path, _root: &Path) -> FormSpanMap {
                HashMap::new()
            }
            fn test_plan(
                &self,
                root: &Path,
                _source: &Path,
                _language: &str,
                _override_cmd: Option<&str>,
                _xcode_scheme: Option<&str>,
                _xcode_destination: Option<&str>,
            ) -> (Command, PathBuf) {
                (Command::Argv(vec!["true".into()]), root.to_path_buf())
            }
        }

        let root = std::env::temp_dir().join(format!("mutator_scan_cov_{}", uuid::Uuid::new_v4()));
        let file = root.join("src").join("demo.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "def f(x):\n    return x + 1\n").unwrap();

        let prov = TwoFormProvider {
            digest_calls: Cell::new(0),
        };
        let mut prior = History::default();
        prior.forms.insert(
            ("demo".into(), "defn/f".into()),
            PriorForm {
                namespace: "demo".into(),
                id: "defn/f".into(),
                digest: "old_f".into(),
                file: "src/demo.py".into(),
            },
        );
        prior.forms.insert(
            ("demo".into(), "defn/g".into()),
            PriorForm {
                namespace: "demo".into(),
                id: "defn/g".into(),
                digest: "old_g".into(),
                file: "src/demo.py".into(),
            },
        );

        // 1. Coverage enabled with absent file (`covered_lines: None`) marks every site uncovered.
        let missing_cov = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: false,
                lines: None,
                cached_sites: None,
            },
            Some(&prior),
            false,
            &prov,
            read_utf8_source,
        )
        .unwrap();
        assert_eq!(
            missing_cov.covered,
            Some(HashMap::from([
                ("m1".to_string(), false),
                ("m2".to_string(), false),
            ]))
        );
        let text = format_scan(
            &missing_cov.file,
            &missing_cov.sites,
            &missing_cov.changed,
            missing_cov.covered.as_ref(),
        );
        assert_eq!(
            text.lines().filter(|l| l.contains(" uncovered")).count(),
            2,
            "expected uncovered label on all sites:\n{text}"
        );
        assert_eq!(prov.digest_calls.get(), 1);

        // 2. Line filter `{2}` restricts `changed` forms to `defn/f` (excluding untouched `defn/g`).
        let line_two = HashSet::from([2usize]);
        let scoped = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: true,
                lines: Some(&line_two),
                cached_sites: None,
            },
            Some(&prior),
            false,
            &prov,
            read_utf8_source,
        )
        .unwrap();
        assert_eq!(scoped.sites.len(), 1);
        assert_eq!(
            scoped.changed,
            HashSet::from([("demo".to_string(), "defn/f".to_string())])
        );
        assert_eq!(prov.digest_calls.get(), 2);

        // 3. Empty line filter skips `form_digests` and returns empty `changed`.
        let empty_line = HashSet::from([99usize]);
        let no_sites = scan_file_core(
            &file,
            &root,
            &ScanOptions {
                covered_lines: None,
                ignore_coverage: true,
                lines: Some(&empty_line),
                cached_sites: None,
            },
            Some(&prior),
            false,
            &prov,
            read_utf8_source,
        )
        .unwrap();
        assert!(no_sites.sites.is_empty());
        assert!(no_sites.changed.is_empty());
        assert_eq!(prov.digest_calls.get(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }
}
