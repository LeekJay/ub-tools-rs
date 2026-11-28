use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::analyze::{analyze_resolved_files_with_cache, source_path};
use crate::coverage::{CoverageBundle, load_bundle};
use crate::discover::{
    CLOJURE_TEST_SUFFIXES, GO_TEST_SUFFIX, JS_TEST_SUFFIXES, SKIP_DIRS, TEST_DIRS,
    VUE_TEST_SUFFIXES, is_test_file, iter_source_files, language_of,
};
pub use crate::git_diff::GitError;
use crate::git_diff::{ChangedLines, changed_scope, entry_overlaps_changes, run_git};
use crate::metrics::write_metrics;
use crate::model::Entry;
use crate::path_util::{resolve_path, to_posix};
use crate::report::{format_json, format_report};
use crate::runners::run_coverage;

pub fn help_text() -> String {
    let mut skipped_set: BTreeSet<&str> = SKIP_DIRS.iter().copied().collect();
    skipped_set.extend(TEST_DIRS.iter().copied());
    let skipped_dirs: String = skipped_set.into_iter().collect::<Vec<_>>().join(", ");

    let mut patterns = vec![format!("*{GO_TEST_SUFFIX}")];
    patterns.extend(CLOJURE_TEST_SUFFIXES.iter().map(|s| format!("*{s}")));
    patterns.extend(JS_TEST_SUFFIXES.iter().map(|s| format!("*{s}")));
    patterns.extend(VUE_TEST_SUFFIXES.iter().map(|s| format!("*{s}")));
    patterns.extend([
        "test_*.py".into(),
        "*_test.py".into(),
        "conftest.py".into(),
        "*Tests.swift".into(),
    ]);
    let test_patterns = patterns.join(", ");

    format!(
        "\
Usage: crapper [options] [path-or-filter ...]

Detect the language of each source file and score it with the CRAP metric
CRAP = CC² × (1 − coverage)³ + CC. Writes .metrics/crap.edn for uml-viewer
and prints a report sorted worst first.

Languages: Clojure (.clj .cljc .cljs .bb), Java (.java), Go (.go),
TypeScript and JavaScript (.ts .tsx .mts .cts .js .jsx .mjs .cjs),
Vue (.vue), Rust (.rs), Python (.py), Swift (.swift).

Options:
  -h, --help                    Print this help and exit.
  --root <path>                 Project root. Metrics are written here.
                                Default: the current directory.
  -s, --source-root <path>      Walk this tree instead of the project root.
                                May be repeated.
  --changed                     Analyze added and modified source files from
                                git status.
  --changed-functions           Analyze only functions overlapping git diff
                                changes (plus all functions in new files;
                                implies --no-metrics).
  --base <ref>                  Git base ref for --changed or
                                --changed-functions (compares <ref> against
                                working tree).
  --no-coverage                 Score complexity only. Coverage and CRAP are N/A.
  --use-existing-coverage       Read coverage already on disk. Do not rerun tests.
  --coverage-command <cmd>      Run this command instead of the per-language
                                coverage tools, then read the reports it wrote.
  --no-metrics                  Skip writing .metrics/crap.edn. Print report only.
  --threshold <number>          Exit 2 when the worst CRAP score is above this.
  --format <fmt>                Output format: text or json. Default: text.
  --json                        Same as --format json.
  --no-cache                    Disable incremental AST caching.

Arguments:
  path              File or directory to analyze. Test paths are included when
                    you name them explicitly.
  filter            When the argument is not a path, only source files whose
                    path contains this text are analyzed.

With no paths, source files under the project root are analyzed. Directories
named {skipped_dirs} are skipped, as are {test_patterns}.

Coverage, when it is produced:
  Clojure      clj -M:cov --lcov, then Cloverage form counts or LCOV
  Java         Maven JaCoCo instruction coverage, for each pom.xml module
  Go           go test ./... -coverprofile=...
  TypeScript   npm run coverage, Vitest's own LCOV, or c8 around npm test
  Vue          npm run coverage, Vitest's own LCOV, or c8 around npm test
  Rust         cargo llvm-cov, or cargo tarpaulin, in the nearest Cargo.toml
  Python       coverage.py LCOV, via pytest or unittest, per project
  Swift        swift test --enable-code-coverage, then llvm-cov JSON;
               or xcodebuild test and xcrun xccov view --report --json

uml-viewer joins .metrics/crap.edn on :namespace and :name.
  Clojure      the ns, and the defn name
  Java         package.Class, and the method name
  Go           import path, or import.path.Receiver for methods
  TypeScript   dotted module path, or module.Class for methods
  Vue          dotted component path, or module.Class for class methods
  Rust         crate::module, or crate::module::Type for methods
  Python       dotted module path, or module.Class for methods
  Swift        the module, or Module.Type for methods and properties
"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub action: String,
    pub message: String,
    pub exit_code: i32,
    pub project_root: PathBuf,
    pub source_roots: Vec<String>,
    pub positionals: Vec<String>,
    pub no_coverage: bool,
    pub no_metrics: bool,
    pub use_existing_coverage: bool,
    pub coverage_command: Option<String>,
    pub threshold: Option<f64>,
    pub changed: bool,
    pub changed_functions: bool,
    pub base: Option<String>,
    pub format: String,
    pub no_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            action: "analyze".to_string(),
            message: String::new(),
            exit_code: 0,
            project_root: PathBuf::from("."),
            source_roots: Vec::new(),
            positionals: Vec::new(),
            no_coverage: false,
            no_metrics: false,
            use_existing_coverage: false,
            coverage_command: None,
            threshold: None,
            changed: false,
            changed_functions: false,
            base: None,
            format: "text".to_string(),
            no_cache: false,
        }
    }
}

fn help_error(msg: impl AsRef<str>, help: &str) -> Options {
    Options {
        action: "help".into(),
        message: format!("{}\n\n{help}", msg.as_ref()),
        exit_code: 1,
        ..Options::default()
    }
}

fn take_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    let next = args
        .get(index + 1)
        .filter(|v| !v.is_empty() && !v.starts_with('-'))
        .ok_or_else(|| format!("{option} requires a value"))?;
    Ok(next.as_str())
}

pub fn parse_args(args: &[String]) -> Options {
    let help = help_text();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Options {
            action: "help".into(),
            message: help,
            exit_code: 0,
            ..Options::default()
        };
    }

    let mut options = Options::default();
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-s" | "--source-root" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.source_roots.push(val.to_string());
                    index += 2;
                }
                Err(msg) => return help_error(msg, &help),
            },
            "--root" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.project_root = PathBuf::from(val);
                    index += 2;
                }
                Err(msg) => return help_error(msg, &help),
            },
            "--coverage-command" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.coverage_command = Some(val.to_string());
                    index += 2;
                }
                Err(msg) => return help_error(msg, &help),
            },
            "--threshold" => match take_value(args, index, arg) {
                Ok(val) => match val.parse::<f64>() {
                    Ok(num) if num.is_finite() => {
                        options.threshold = Some(num);
                        index += 2;
                    }
                    _ => return help_error("--threshold requires a number", &help),
                },
                Err(msg) => return help_error(msg, &help),
            },
            "--format" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.format = val.to_string();
                    index += 2;
                }
                Err(msg) => return help_error(msg, &help),
            },
            "--json" => {
                options.format = "json".to_string();
                index += 1;
            }
            "--no-cache" => {
                options.no_cache = true;
                index += 1;
            }
            "--no-coverage" => {
                options.no_coverage = true;
                index += 1;
            }
            "--no-metrics" => {
                options.no_metrics = true;
                index += 1;
            }
            "--use-existing-coverage" => {
                options.use_existing_coverage = true;
                index += 1;
            }
            "--changed" => {
                options.changed = true;
                index += 1;
            }
            "--changed-functions" => {
                options.changed_functions = true;
                index += 1;
            }
            "--base" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.base = Some(val.to_string());
                    index += 2;
                }
                Err(msg) => return help_error(msg, &help),
            },
            s if s.starts_with('-') => return help_error(format!("Unknown option: {s}"), &help),
            _ => {
                options.positionals.push(arg.clone());
                index += 1;
            }
        }
    }

    if options.base.is_some() && !options.changed && !options.changed_functions {
        return help_error("--base requires --changed or --changed-functions", &help);
    }

    if options.format != "text" && options.format != "json" {
        return help_error(format!("Unknown format: {}", options.format), &help);
    }

    if options.no_coverage
        && options
            .coverage_command
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    {
        return help_error(
            "--no-coverage cannot be combined with --coverage-command",
            &help,
        );
    }

    options.no_metrics = options.no_metrics || ub_runtime::env_no_metrics();
    if options.changed_functions {
        options.no_metrics = true;
    }
    if options.action != "help" && ub_runtime::env_no_cache() {
        options.no_cache = true;
    }
    options
}

fn changed_files(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    let (top_code, top_out, top_err) = run_git(root, &["rev-parse", "--show-toplevel"]);
    if top_code != 0 {
        let msg = top_err.trim();
        return Err(GitError {
            code: top_code,
            message: if msg.is_empty() {
                "git status failed".to_string()
            } else {
                msg.to_string()
            },
        });
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
        let msg = st_err.trim();
        return Err(GitError {
            code: st_code,
            message: if msg.is_empty() {
                "git status failed".to_string()
            } else {
                msg.to_string()
            },
        });
    }
    let top_text = String::from_utf8_lossy(&top_out).trim().to_string();
    let top = PathBuf::from(top_text);
    let limit = resolve_path(root);
    let mut found = Vec::new();
    for entry in st_out.split(|&b| b == 0) {
        if entry.len() < 4 {
            continue;
        }
        let rel_str = String::from_utf8_lossy(&entry[3..]);
        let path = resolve_path(&top.join(rel_str.as_ref()));
        if path.is_file() && path.starts_with(&limit) {
            found.push(path);
        }
    }
    Ok(found)
}

fn positionals(root: &Path, args: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut existing = Vec::new();
    let mut filters = Vec::new();
    for arg in args {
        let candidate = Path::new(arg);
        let full = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root.join(arg)
        };
        if full.exists() {
            existing.push(resolve_path(&full));
        } else {
            filters.push(arg.clone());
        }
    }
    (existing, filters)
}

fn changed_source(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    let files = changed_files(root)?;
    Ok(files
        .into_iter()
        .filter(|p| language_of(p).is_some() && !is_test_file(p))
        .collect())
}

fn explicit_files(existing: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in existing {
        if path.is_dir() {
            files.extend(iter_source_files(std::slice::from_ref(path)));
        } else if language_of(path).is_some() {
            files.push(path.clone());
        }
    }
    files
}

fn selected(
    options: &Options,
    root: &Path,
    existing: &[PathBuf],
) -> Result<Vec<PathBuf>, GitError> {
    if options.changed {
        return changed_source(root);
    }
    if !options.source_roots.is_empty() {
        let starts: Vec<PathBuf> = options
            .source_roots
            .iter()
            .map(|p| resolve_path(&root.join(p)))
            .collect();
        return Ok(iter_source_files(&starts));
    }
    if !existing.is_empty() {
        return Ok(explicit_files(existing));
    }
    Ok(iter_source_files(&[root.to_path_buf()]))
}

fn apply_filters(files: Vec<PathBuf>, filters: &[String]) -> Vec<PathBuf> {
    if filters.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter(|path| {
            let posix = to_posix(path);
            filters.iter().any(|item| posix.contains(item.as_str()))
        })
        .collect()
}

fn finalize_selected(files: Vec<PathBuf>, filters: &[String]) -> Vec<PathBuf> {
    let picked = apply_filters(files, filters);
    let mut unique: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for resolved in picked {
        unique.insert((to_posix(&resolved), resolved));
    }
    unique.into_iter().map(|(_, p)| p).collect()
}

fn filter_by_existing(files: Vec<PathBuf>, existing: &[PathBuf]) -> Vec<PathBuf> {
    if existing.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter(|path| existing.iter().any(|e| path == e || path.starts_with(e)))
        .collect()
}

struct ChangedSelection {
    files: Vec<PathBuf>,
    lines_by_path: Option<HashMap<String, ChangedLines>>,
}

fn resolve_selection(options: &Options, root: &Path) -> Result<ChangedSelection, GitError> {
    let (existing, filters) = positionals(root, &options.positionals);
    if options.changed_functions {
        let scope = changed_scope(root, options.base.as_deref())?;
        let mut lines_by_path = HashMap::with_capacity(scope.len());
        let mut raw_files = Vec::with_capacity(scope.len());
        for (path, lines) in scope {
            lines_by_path.insert(source_path(&path, root), lines);
            raw_files.push(path);
        }
        let filtered = filter_by_existing(raw_files, &existing);
        return Ok(ChangedSelection {
            files: finalize_selected(filtered, &filters),
            lines_by_path: Some(lines_by_path),
        });
    }
    if options.changed && options.base.is_some() {
        let scope = changed_scope(root, options.base.as_deref())?;
        let raw_files: Vec<PathBuf> = scope.into_iter().map(|(path, _)| path).collect();
        let filtered = filter_by_existing(raw_files, &existing);
        return Ok(ChangedSelection {
            files: finalize_selected(filtered, &filters),
            lines_by_path: None,
        });
    }
    let raw_files = selected(options, root, &existing)?;
    Ok(ChangedSelection {
        files: finalize_selected(raw_files, &filters),
        lines_by_path: None,
    })
}

pub fn select_files(options: &Options) -> Result<Vec<PathBuf>, GitError> {
    let root = resolve_path(&options.project_root);
    resolve_selection(options, &root).map(|selection| selection.files)
}

fn coverage_bundle(options: &Options, root: &Path, files: &[PathBuf]) -> Option<CoverageBundle> {
    if options.no_coverage {
        return None;
    }
    if options.use_existing_coverage {
        return Some(load_bundle(root));
    }
    let _cov_lock = ub_runtime::acquire_repo_lock(
        "coverage",
        root,
        Some(&format!(
            "Waiting for another coverage run on {}...",
            root.display()
        )),
    )
    .ok();
    let status = run_coverage(root, files, options.coverage_command.as_deref());
    if options
        .coverage_command
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        && status != 0
    {
        return Some(CoverageBundle::default());
    }
    Some(load_bundle(root))
}

fn fn_word(count: usize) -> &'static str {
    if count == 1 { "function" } else { "functions" }
}

/// Warn on stderr when analyzed files are missing from coverage reports or newer than the latest report.
fn write_coverage_freshness_warnings(
    err: &mut dyn Write,
    root: &Path,
    bundle: Option<&CoverageBundle>,
    entries: &[Entry],
) {
    let Some(bundle) = bundle else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    let mut by_file: BTreeMap<&str, bool> = BTreeMap::new();
    for entry in entries {
        let covered = bundle.has_coverage_for_entry(
            &entry.path,
            &entry.language,
            &entry.namespace,
            &entry.name,
        );
        by_file
            .entry(entry.path.as_str())
            .and_modify(|seen| *seen |= covered)
            .or_insert(covered);
    }
    let total = by_file.len();
    let file_word = if total == 1 { "file" } else { "files" };
    let missing: Vec<&str> = by_file
        .iter()
        .filter_map(|(&path, &covered)| (!covered).then_some(path))
        .collect();
    if !missing.is_empty() {
        let sample = missing[0];
        let _ = writeln!(
            err,
            "[crapper] WARN: {}/{total} analyzed {file_word} missing from coverage report (scored as 0% coverage; e.g. {sample}). Re-run coverage if reports are stale.",
            missing.len(),
        );
    }
    if let Some(report_mtime) = bundle.newest_report_mtime {
        let stale: Vec<&str> = by_file
            .iter()
            .filter_map(|(&path, &covered)| {
                if !covered {
                    return None;
                }
                let full = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    root.join(path)
                };
                let src_mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok()?;
                (src_mtime > report_mtime).then_some(path)
            })
            .collect();
        if !stale.is_empty() {
            let sample = stale[0];
            let _ = writeln!(
                err,
                "[crapper] WARN: coverage report is older than {}/{total} analyzed {file_word} (e.g. {sample}); line coverage and CRAP scores may be stale. Re-run coverage to refresh.",
                stale.len(),
            );
        }
    }
}

fn threshold_status(options: &Options, entries: &[Entry], err: &mut dyn Write) -> i32 {
    let total = entries.len();
    let scored_count = entries.iter().filter(|e| e.crap.is_some()).count();
    let worst_scored = entries
        .iter()
        .filter_map(|e| e.crap.map(|crap| (crap, e)))
        .max_by(|a, b| a.0.total_cmp(&b.0));

    let Some(threshold) = options.threshold else {
        if let Some((max_crap, worst)) = worst_scored {
            let word = fn_word(scored_count);
            let _ = writeln!(
                err,
                "[crapper] INFO (exit 0): {scored_count} {word} scored (worst: {}:{} {} CRAP={max_crap:.1}; no --threshold set).",
                worst.path, worst.start_line, worst.name,
            );
        } else if !entries.is_empty() {
            let word = fn_word(total);
            let max_cc = entries.iter().map(|e| e.complexity).max().unwrap_or(0);
            let _ = writeln!(
                err,
                "[crapper] INFO (exit 0): {total} {word} analyzed without coverage (CRAP=N/A; max CC={max_cc})."
            );
        }
        return 0;
    };

    let over_count = entries
        .iter()
        .filter(|e| e.crap.is_some_and(|c| c > threshold))
        .count();
    if let Some((max_crap, worst)) = worst_scored
        && max_crap > threshold
    {
        let word = fn_word(over_count);
        let cov_text = worst
            .coverage
            .map(|c| format!("{c:.1}%"))
            .unwrap_or_else(|| "N/A".to_string());
        let _ = writeln!(
            err,
            "[crapper] FAIL (exit 2): CRAP threshold exceeded: {max_crap:.1} > {threshold:.1} ({over_count}/{total} {word} over limit; worst: {}:{} {} CRAP={max_crap:.1} CC={} cov={cov_text}). Split complex functions or add tests.",
            worst.path, worst.start_line, worst.name, worst.complexity,
        );
        return 2;
    }
    if let Some((max_crap, worst)) = worst_scored {
        let word = fn_word(scored_count);
        let _ = writeln!(
            err,
            "[crapper] PASS (exit 0): all {scored_count} scored {word} <= CRAP {threshold:.1} (worst: {}:{} {} CRAP={max_crap:.1}).",
            worst.path, worst.start_line, worst.name,
        );
    } else if !entries.is_empty() {
        let word = fn_word(total);
        let _ = writeln!(
            err,
            "[crapper] WARN (exit 0): {total} {word} analyzed without coverage (CRAP=N/A; --threshold {threshold:.1} not evaluated)."
        );
    } else {
        let _ = writeln!(
            err,
            "[crapper] PASS (exit 0): 0 functions scored (threshold <= {threshold:.1})."
        );
    }
    0
}

pub fn run_with_io(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let options = parse_args(args);
    if options.action == "help" {
        let stream: &mut dyn Write = if options.exit_code == 0 { out } else { err };
        let _ = write!(stream, "{}", options.message);
        if !options.message.ends_with('\n') {
            let _ = writeln!(stream);
        }
        return options.exit_code;
    }

    let root = resolve_path(&options.project_root);
    let selection = match resolve_selection(&options, &root) {
        Ok(s) => s,
        Err(exc) => {
            let _ = writeln!(err, "{}", exc.message);
            return exc.code;
        }
    };
    let files = selection.files;
    if files.is_empty() {
        if options.format == "json" {
            let _ = write!(out, "{}", format_json(&[], options.threshold));
            let _ = out.flush();
        } else {
            let _ = writeln!(out, "No source files to analyze.");
        }
        let _ = writeln!(err, "[crapper] INFO (exit 0): 0 matching source files.");
        return 0;
    }

    let _slot = ub_runtime::acquire_global_scan_slot().ok().flatten();

    let mut bundle = coverage_bundle(&options, &root, &files);
    let mut entries =
        analyze_resolved_files_with_cache(&files, &root, bundle.as_mut(), !options.no_cache);
    let mut untouched_over_threshold = 0usize;
    let mut untouched_worst: Option<(String, usize, String, f64)> = None;
    if let Some(ref changed_map) = selection.lines_by_path {
        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry_overlaps_changes(&entry, changed_map) {
                kept.push(entry);
            } else if let Some(threshold) = options.threshold
                && let Some(crap) = entry.crap
                && crap > threshold
            {
                untouched_over_threshold += 1;
                let replace = untouched_worst
                    .as_ref()
                    .is_none_or(|(_, _, _, worst)| crap > *worst);
                if replace {
                    untouched_worst = Some((entry.path, entry.start_line, entry.name, crap));
                }
            }
        }
        entries = kept;
    }
    let written_path = if options.no_metrics {
        None
    } else {
        match write_metrics(&entries, &root) {
            Ok(path) => Some(path),
            Err(exc) => {
                let _ = writeln!(err, "Failed to write metrics: {exc}");
                return 1;
            }
        }
    };
    let report = if options.format == "json" {
        format_json(&entries, options.threshold)
    } else {
        format_report(&entries)
    };
    let _ = write!(out, "{report}");
    let _ = out.flush();
    if let Some(metrics) = written_path {
        let _ = writeln!(err, "Wrote {}", metrics.display());
    }
    write_coverage_freshness_warnings(err, &root, bundle.as_ref(), &entries);
    if untouched_over_threshold > 0
        && let (Some(threshold), Some((u_path, u_line, u_name, u_crap))) =
            (options.threshold, untouched_worst)
    {
        let word = fn_word(untouched_over_threshold);
        let _ = writeln!(
            err,
            "[crapper] INFO (non-blocking, not in exit code): {untouched_over_threshold} untouched {word} in changed files exceed CRAP {threshold:.1} (worst: {u_path}:{u_line} {u_name} CRAP={u_crap:.1}). Existing tech debt only; record in Limitations, do not fail this change."
        );
    }
    threshold_status(&options, &entries, err)
}

pub fn run(args: &[String]) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with_io(args, &mut stdout, &mut stderr)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::metrics::metrics_path;

    use super::*;

    const SUBPROCESS_MARKER: &str = "CRAPPER_TEST_NO_METRICS_SUBPROC";
    const SUBPROCESS_EXPECT_ENV: &str = "CRAPPER_TEST_EXPECT_NO_METRICS";

    static NEXT_DIR_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crapper-cli-{label}-{}-{nanos}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test temp dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_branchy_fixture(dir: &TestDir) {
        let src_dir = dir.0.join("src");
        let cov_dir = dir.0.join("coverage");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::create_dir_all(&cov_dir).expect("create coverage dir");
        fs::write(
            src_dir.join("app.py"),
            "def branchy(x):\n    if x > 0:\n        return 1\n    return 0\n",
        )
        .expect("write python fixture");
        fs::write(
            cov_dir.join("lcov.info"),
            "SF:src/app.py\nDA:1,0\nDA:2,0\nDA:3,0\nDA:4,0\nend_of_record\n",
        )
        .expect("write lcov fixture");
    }

    fn run_fixture_cli(
        dir: &TestDir,
        extra_args: &[&str],
        threshold: &str,
    ) -> (i32, String, String) {
        let root_str = dir.0.to_string_lossy().into_owned();
        let mut args = vec![
            "--root".to_string(),
            root_str,
            "--use-existing-coverage".to_string(),
            "--threshold".to_string(),
            threshold.to_string(),
        ];
        args.extend(extra_args.iter().map(|s| (*s).to_string()));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    fn assert_no_metrics_behavior(dir: &TestDir, extra_args: &[&str]) {
        let (code_exceeded, out_exceeded, err_exceeded) = run_fixture_cli(dir, extra_args, "5.0");
        assert_eq!(code_exceeded, 2, "expected threshold exit code 2");
        assert!(out_exceeded.contains("CRAP Report"));
        assert!(out_exceeded.contains("branchy"));
        assert!(err_exceeded.contains("CRAP threshold exceeded:"));
        assert!(!err_exceeded.contains("Wrote "));
        assert!(!metrics_path(&dir.0).exists());
        assert!(!dir.0.join(".metrics").exists());

        let (code_ok, out_ok, err_ok) = run_fixture_cli(dir, extra_args, "10.0");
        assert_eq!(code_ok, 0, "expected exit code 0 below threshold");
        assert!(out_ok.contains("CRAP Report"));
        assert!(out_ok.contains("branchy"));
        assert!(!err_ok.contains("Wrote "));
        assert!(!metrics_path(&dir.0).exists());
        assert!(!dir.0.join(".metrics").exists());
    }

    fn run_self_test(test_name: &str, envs: &[(&str, Option<&str>)]) {
        let exe = std::env::current_exe().expect("current test exe");
        let mut cmd = Command::new(exe);
        cmd.args(["--exact", test_name, "--nocapture"]);
        cmd.env(SUBPROCESS_MARKER, "1");
        for (key, value) in envs {
            match value {
                Some(v) => {
                    cmd.env(key, v);
                }
                None => {
                    cmd.env_remove(key);
                }
            }
        }
        let output = cmd.output().expect("spawn subprocess self-test");
        assert!(
            output.status.success(),
            "subprocess test {test_name} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn options_default_and_help_include_no_metrics() {
        assert!(!Options::default().no_metrics);
        assert!(help_text().contains("--no-metrics"));

        let parsed = parse_args(&["--no-metrics".to_string()]);
        assert_eq!(parsed.action, "analyze");
        assert!(parsed.no_metrics);

        let help = parse_args(&["--help".to_string()]);
        assert_eq!(help.action, "help");
        assert!(!help.no_metrics);
    }

    #[test]
    fn no_metrics_flag_skips_metrics_file_and_preserves_report_and_threshold() {
        let dir = TestDir::new("flag");
        create_branchy_fixture(&dir);
        assert_no_metrics_behavior(&dir, &["--no-metrics"]);
    }

    #[test]
    fn ub_tools_no_metrics_env_behaves_identically_in_subprocesses() {
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            let expect_no_metrics = std::env::var(SUBPROCESS_EXPECT_ENV).as_deref() == Ok("1");
            let parsed = parse_args(&[]);
            assert_eq!(parsed.action, "analyze");
            assert_eq!(parsed.no_metrics, expect_no_metrics);

            let help = parse_args(&["--help".to_string()]);
            assert_eq!(help.action, "help");
            assert!(!help.no_metrics);

            let dir = TestDir::new("env-subproc");
            create_branchy_fixture(&dir);
            if expect_no_metrics {
                assert_no_metrics_behavior(&dir, &[]);
            } else {
                let (code, out, err) = run_fixture_cli(&dir, &[], "5.0");
                assert_eq!(code, 2);
                assert!(out.contains("CRAP Report"));
                assert!(err.contains("Wrote "));
                assert!(err.contains("CRAP threshold exceeded:"));
                assert!(metrics_path(&dir.0).is_file());
            }
            return;
        }

        for (env_val, expect) in [
            (None, "0"),
            (Some("false"), "0"),
            (Some("1"), "1"),
            (Some("true"), "1"),
        ] {
            run_self_test(
                "cli::tests::ub_tools_no_metrics_env_behaves_identically_in_subprocesses",
                &[
                    ("UB_TOOLS_NO_METRICS", env_val),
                    (SUBPROCESS_EXPECT_ENV, Some(expect)),
                ],
            );
        }
    }

    #[test]
    fn options_and_help_support_format_json_and_no_cache() {
        let defaults = Options::default();
        assert_eq!(defaults.format, "text");
        assert!(!defaults.no_cache);

        let help = help_text();
        assert!(help.contains("--format"));
        assert!(help.contains("--json"));
        assert!(help.contains("--no-cache"));

        let json_flag = parse_args(&["--json".to_string(), "--no-cache".to_string()]);
        assert_eq!(json_flag.action, "analyze");
        assert_eq!(json_flag.format, "json");
        assert!(json_flag.no_cache);

        let format_json_opt = parse_args(&["--format".to_string(), "json".to_string()]);
        assert_eq!(format_json_opt.action, "analyze");
        assert_eq!(format_json_opt.format, "json");

        let format_text_opt = parse_args(&["--format".to_string(), "text".to_string()]);
        assert_eq!(format_text_opt.action, "analyze");
        assert_eq!(format_text_opt.format, "text");

        let bad_format = parse_args(&["--format".to_string(), "yaml".to_string()]);
        assert_eq!(bad_format.action, "help");
        assert_eq!(bad_format.exit_code, 1);
        assert!(bad_format.message.contains("Unknown format: yaml"));
        assert!(bad_format.message.contains("Usage: crapper"));

        let missing_format = parse_args(&["--format".to_string()]);
        assert_eq!(missing_format.action, "help");
        assert_eq!(missing_format.exit_code, 1);
    }

    #[test]
    fn cli_json_output_handles_empty_scored_threshold_and_unknown_format() {
        let empty_dir = TestDir::new("json-empty");
        let empty_root = empty_dir.0.to_string_lossy().into_owned();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            &["--root".into(), empty_root.clone(), "--json".into()],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&err)
                .contains("[crapper] INFO (exit 0): 0 matching source files.")
        );
        let empty_json: serde_json::Value =
            serde_json::from_slice(&out).expect("empty --json stdout should parse");
        assert_eq!(empty_json["version"], 1);
        assert_eq!(empty_json["tool"], "crapper");
        assert_eq!(empty_json["summary"]["functions"], 0);
        assert_eq!(empty_json["entries"], serde_json::json!([]));

        let mut bad_out = Vec::new();
        let mut bad_err = Vec::new();
        let bad_code = run_with_io(
            &[
                "--root".into(),
                empty_root,
                "--format".into(),
                "yaml".into(),
            ],
            &mut bad_out,
            &mut bad_err,
        );
        assert_eq!(bad_code, 1);
        assert!(bad_out.is_empty());
        let bad_err_text = String::from_utf8_lossy(&bad_err);
        assert!(bad_err_text.contains("Unknown format: yaml"));
        assert!(bad_err_text.contains("Usage: crapper"));

        let scored_dir = TestDir::new("json-scored");
        create_branchy_fixture(&scored_dir);
        let (exceeded_code, exceeded_out, exceeded_err) =
            run_fixture_cli(&scored_dir, &["--format", "json", "--no-metrics"], "5.0");
        assert_eq!(exceeded_code, 2);
        let exceeded_val: serde_json::Value =
            serde_json::from_str(&exceeded_out).expect("valid json on threshold exceeded");
        assert_eq!(exceeded_val["version"], 1);
        assert_eq!(exceeded_val["tool"], "crapper");
        assert_eq!(exceeded_val["languages"], serde_json::json!(["python"]));
        assert_eq!(exceeded_val["summary"]["functions"], 1);
        assert_eq!(exceeded_val["summary"]["threshold"], 5.0);
        assert_eq!(exceeded_val["summary"]["threshold_exceeded"], true);
        assert_eq!(exceeded_val["entries"][0]["name"], "branchy");
        assert!(exceeded_err.contains("[crapper] FAIL (exit 2): CRAP threshold exceeded:"));
        assert!(exceeded_err.contains("worst:"));
        assert!(!metrics_path(&scored_dir.0).exists());

        let (ok_code, ok_out, ok_err) = run_fixture_cli(&scored_dir, &["--json"], "10.0");
        assert_eq!(ok_code, 0);
        let ok_val: serde_json::Value =
            serde_json::from_str(&ok_out).expect("valid json below threshold");
        assert_eq!(ok_val["summary"]["threshold_exceeded"], false);
        assert!(ok_err.contains("[crapper] PASS (exit 0): all 1 scored function <= CRAP 10.0"));
        assert!(metrics_path(&scored_dir.0).is_file());

        let (nocov_code, _, nocov_err) =
            run_fixture_cli(&scored_dir, &["--no-coverage", "--no-metrics"], "10.0");
        assert_eq!(nocov_code, 0);
        assert!(nocov_err.contains("[crapper] WARN (exit 0):"));
        assert!(nocov_err.contains("not evaluated"));
    }

    #[test]
    fn no_cache_flag_and_env_skip_cache_writes_in_subprocesses() {
        const CACHE_SUBPROC: &str = "CRAPPER_TEST_NO_CACHE_SUBPROC";
        const CACHE_MODE_ENV: &str = "CRAPPER_TEST_CACHE_MODE";

        if std::env::var_os(CACHE_SUBPROC).is_some() {
            let mode = std::env::var(CACHE_MODE_ENV).expect("cache mode");
            let dir = TestDir::new("no-cache-subproc");
            create_branchy_fixture(&dir);
            let cache_file = crate::cache::cache_path(&resolve_path(&dir.0));

            let help = parse_args(&["--help".to_string()]);
            assert_eq!(help.action, "help");
            assert!(!help.no_cache);

            match mode.as_str() {
                "flag" => {
                    assert!(!parse_args(&[]).no_cache);
                    assert!(parse_args(&["--no-cache".to_string()]).no_cache);
                    let (code, _, _) =
                        run_fixture_cli(&dir, &["--no-cache", "--no-metrics"], "10.0");
                    assert_eq!(code, 0);
                    assert!(!cache_file.exists());
                }
                "env" => {
                    assert!(parse_args(&[]).no_cache);
                    let (code, _, _) = run_fixture_cli(&dir, &["--no-metrics"], "10.0");
                    assert_eq!(code, 0);
                    assert!(!cache_file.exists());
                }
                "enabled" => {
                    assert!(!parse_args(&[]).no_cache);
                    let (code, _, _) = run_fixture_cli(&dir, &["--no-metrics"], "10.0");
                    assert_eq!(code, 0);
                    assert!(cache_file.is_file());
                }
                other => panic!("unexpected mode: {other}"),
            }
            return;
        }

        let dir = TestDir::new("no-cache-host");
        let exe = std::env::current_exe().expect("current test exe");
        for (idx, (mode, no_cache_env)) in [("flag", None), ("env", Some("1")), ("enabled", None)]
            .into_iter()
            .enumerate()
        {
            let cache_dir = dir.0.join(format!("cache-{idx}"));
            let mut cmd = Command::new(&exe);
            cmd.args([
                "--exact",
                "cli::tests::no_cache_flag_and_env_skip_cache_writes_in_subprocesses",
                "--nocapture",
            ]);
            cmd.env(CACHE_SUBPROC, "1");
            cmd.env(CACHE_MODE_ENV, mode);
            cmd.env("UB_TOOLS_CACHE_DIR", &cache_dir);
            match no_cache_env {
                Some(v) => {
                    cmd.env("UB_TOOLS_NO_CACHE", v);
                }
                None => {
                    cmd.env_remove("UB_TOOLS_NO_CACHE");
                }
            }
            let out = cmd.output().expect("spawn no-cache subprocess");
            assert!(
                out.status.success(),
                "subprocess mode {mode} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn options_and_help_support_changed_functions_and_base() {
        let help = help_text();
        assert!(help.contains("--changed-functions"));
        assert!(help.contains("implies --no-metrics"));
        assert!(help.contains("--base <ref>"));

        let fn_only = parse_args(&["--changed-functions".to_string()]);
        assert_eq!(fn_only.action, "analyze");
        assert!(fn_only.changed_functions);
        assert!(fn_only.no_metrics);
        assert_eq!(fn_only.base, None);

        let with_base = parse_args(&[
            "--changed-functions".to_string(),
            "--base".to_string(),
            "main".to_string(),
        ]);
        assert_eq!(with_base.action, "analyze");
        assert!(with_base.changed_functions);
        assert_eq!(with_base.base.as_deref(), Some("main"));

        let base_alone = parse_args(&["--base".to_string(), "main".to_string()]);
        assert_eq!(base_alone.action, "help");
        assert_eq!(base_alone.exit_code, 1);
        assert!(
            base_alone
                .message
                .contains("--base requires --changed or --changed-functions")
        );

        let missing_base = parse_args(&["--changed-functions".to_string(), "--base".to_string()]);
        assert_eq!(missing_base.action, "help");
        assert_eq!(missing_base.exit_code, 1);
    }

    #[test]
    fn changed_functions_filters_untouched_legacy_and_supports_base_and_untracked() {
        fn git_ok(root: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let dir = TestDir::new("git-changed-fns");
        let root = &dir.0;
        git_ok(root, &["init"]);
        git_ok(root, &["config", "user.name", "Test User"]);
        git_ok(root, &["config", "user.email", "test@example.com"]);

        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("coverage")).unwrap();

        let initial_py = concat!(
            "# header comment\n",
            "def legacy_giant(a, b, c, d):\n",
            "    if a > 0 and b > 0:\n",
            "        if c > 0 or d > 0:\n",
            "            return 1\n",
            "        return 2\n",
            "    return 0\n",
            "\n",
            "def small_ok(x):\n",
            "    return x + 1\n",
        );
        fs::write(root.join("src/app.py"), initial_py).unwrap();
        fs::write(
            root.join("coverage/lcov.info"),
            "SF:src/app.py\nDA:2,0\nDA:3,0\nDA:4,0\nDA:5,0\nDA:6,0\nDA:7,0\nDA:9,1\nDA:10,1\nend_of_record\n",
        )
        .unwrap();

        git_ok(root, &["add", "."]);
        git_ok(root, &["commit", "-m", "initial"]);

        // Modify only `small_ok` in working tree.
        let edited_py = concat!(
            "# header comment\n",
            "def legacy_giant(a, b, c, d):\n",
            "    if a > 0 and b > 0:\n",
            "        if c > 0 or d > 0:\n",
            "            return 1\n",
            "        return 2\n",
            "    return 0\n",
            "\n",
            "def small_ok(x):\n",
            "    return x + 2\n",
        );
        fs::write(root.join("src/app.py"), edited_py).unwrap();

        let (whole_code, whole_out, _) =
            run_fixture_cli(&dir, &["--changed", "--no-metrics", "--json"], "10.0");
        assert_eq!(
            whole_code, 2,
            "whole-file --changed should fail on legacy_giant"
        );
        let whole_json: serde_json::Value = serde_json::from_str(&whole_out).unwrap();
        assert_eq!(whole_json["summary"]["functions"], 2);

        let (fn_code, fn_out, fn_err) =
            run_fixture_cli(&dir, &["--changed-functions", "--json"], "10.0");
        assert_eq!(
            fn_code, 0,
            "--changed-functions should ignore untouched legacy_giant: {fn_out}"
        );
        assert!(
            !metrics_path(root).exists(),
            "--changed-functions must imply --no-metrics and not write .metrics/crap.edn"
        );
        assert!(
            fn_err.contains("[crapper] INFO (non-blocking, not in exit code): 1 untouched function in changed files exceed CRAP 10.0 (worst: src/app.py:2 legacy_giant CRAP="),
            "expected untouched legacy reminder in stderr, got: {fn_err}"
        );
        assert!(
            fn_err.contains("[crapper] PASS (exit 0): all 1 scored function <= CRAP 10.0"),
            "expected pass verdict in stderr, got: {fn_err}"
        );
        assert!(
            !fn_err.contains("missing from coverage report"),
            "covered file should not emit missing coverage warning: {fn_err}"
        );
        let fn_json: serde_json::Value = serde_json::from_str(&fn_out).unwrap();
        assert_eq!(fn_json["summary"]["functions"], 1);
        assert_eq!(fn_json["entries"][0]["name"], "small_ok");
        assert_eq!(fn_json["entries"][0]["start_line"], 9);
        assert_eq!(fn_json["entries"][0]["end_line"], 10);

        let (high_code, _, high_err) =
            run_fixture_cli(&dir, &["--changed-functions", "--json"], "100.0");
        assert_eq!(high_code, 0);
        assert!(
            !high_err.contains("non-blocking"),
            "unexpected reminder below high threshold: {high_err}"
        );

        // Add untracked new file and verify `--changed-functions` picks up its functions
        // and warns when that file is absent from coverage reports.
        fs::write(
            root.join("src/new_mod.py"),
            "def brand_new(y):\n    return y * 2\n",
        )
        .unwrap();
        let (untracked_code, untracked_out, untracked_err) = run_fixture_cli(
            &dir,
            &["--changed-functions", "--no-metrics", "--json"],
            "10.0",
        );
        assert_eq!(untracked_code, 0);
        assert!(
            untracked_err.contains(
                "[crapper] WARN: 1/2 analyzed files missing from coverage report (scored as 0% coverage; e.g. src/new_mod.py)."
            ),
            "expected missing coverage warning in stderr, got: {untracked_err}"
        );
        let (_, _, nocov_err) = run_fixture_cli(
            &dir,
            &[
                "--changed-functions",
                "--no-coverage",
                "--no-metrics",
                "--json",
            ],
            "10.0",
        );
        assert!(
            !nocov_err.contains("missing from coverage report"),
            "--no-coverage must not warn about missing coverage: {nocov_err}"
        );
        let untracked_json: serde_json::Value = serde_json::from_str(&untracked_out).unwrap();
        let names: Vec<&str> = untracked_json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"small_ok"));
        assert!(names.contains(&"brand_new"));

        // Commit changes and verify `--base HEAD~1` works post-commit.
        git_ok(root, &["add", "."]);
        git_ok(root, &["commit", "-m", "edit small_ok and add new_mod"]);
        let (base_code, base_out, _) = run_fixture_cli(
            &dir,
            &[
                "--changed-functions",
                "--base",
                "HEAD~1",
                "--no-metrics",
                "--json",
            ],
            "10.0",
        );
        assert_eq!(base_code, 0, "--base HEAD~1 output: {base_out}");
        let base_json: serde_json::Value = serde_json::from_str(&base_out).unwrap();
        let base_names: Vec<&str> = base_json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(base_names.len(), 2);
        assert!(base_names.contains(&"small_ok"));
        assert!(base_names.contains(&"brand_new"));

        // Edit only top-level comment outside functions -> empty entries, exit 0.
        let comment_only = edited_py.replace("# header comment", "# updated comment");
        fs::write(root.join("src/app.py"), comment_only).unwrap();
        let (comment_code, comment_out, _) = run_fixture_cli(
            &dir,
            &["--changed-functions", "--no-metrics", "--json"],
            "10.0",
        );
        assert_eq!(comment_code, 0);
        let comment_json: serde_json::Value = serde_json::from_str(&comment_out).unwrap();
        assert_eq!(comment_json["summary"]["functions"], 0);
        assert_eq!(comment_json["entries"], serde_json::json!([]));
    }

    #[test]
    fn warns_when_coverage_report_is_older_than_analyzed_sources() {
        let dir = TestDir::new("stale-cov");
        let root = &dir.0;
        let src = root.join("src").join("app.py");
        let lcov = root.join("coverage").join("lcov.info");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::create_dir_all(lcov.parent().unwrap()).unwrap();

        fs::write(&src, "def small(x):\n    return x + 1\n").unwrap();
        fs::write(&lcov, "SF:src/app.py\nDA:1,1\nDA:2,1\nend_of_record\n").unwrap();

        let set_mtime = |path: &Path, secs: u64| {
            let t = UNIX_EPOCH + std::time::Duration::from_secs(secs);
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };

        // 1. Report 60s older than analyzed source triggers stale-coverage WARN (without missing-file WARN).
        set_mtime(&lcov, 1_700_000_000);
        set_mtime(&src, 1_700_000_060);
        let (stale_code, _, stale_err) = run_fixture_cli(&dir, &["--no-metrics"], "10.0");
        assert_eq!(stale_code, 0);
        assert!(
            stale_err.contains(
                "[crapper] WARN: coverage report is older than 1/1 analyzed file (e.g. src/app.py); line coverage and CRAP scores may be stale."
            ),
            "expected stale warning, got:\n{stale_err}"
        );
        assert!(!stale_err.contains("missing from coverage report"));

        // 2. Newer report silences stale-coverage WARN cleanly.
        set_mtime(&lcov, 1_700_000_120);
        let (fresh_code, _, fresh_err) = run_fixture_cli(&dir, &["--no-metrics"], "10.0");
        assert_eq!(fresh_code, 0);
        assert!(
            !fresh_err.contains("coverage report is older than"),
            "unexpected stale warning when report is newer:\n{fresh_err}"
        );
        assert!(!fresh_err.contains("missing from coverage report"));

        // 3. `--no-coverage` never warns about stale coverage reports even when lcov is older.
        set_mtime(&lcov, 1_700_000_000);
        let (nocov_code, _, nocov_err) =
            run_fixture_cli(&dir, &["--no-coverage", "--no-metrics"], "10.0");
        assert_eq!(nocov_code, 0);
        assert!(
            !nocov_err.contains("coverage report is older than"),
            "--no-coverage must not emit stale warning:\n{nocov_err}"
        );
    }
}
