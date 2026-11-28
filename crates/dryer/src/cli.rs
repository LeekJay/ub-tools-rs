//! Command line parsing, git-changed file selection, and report orchestration.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::discover::{
    SKIP_DIRS, TEST_DIRS, is_test_file, iter_source_files, language_of, resolve_path, to_posix,
};
use super::report::{format_json, format_text, render_edn, write_metrics};
use super::scan::scan_and_find_duplicates_resolved;

pub fn skipped_directory_text() -> String {
    let mut ordered: Vec<&str> = SKIP_DIRS.iter().chain(TEST_DIRS.iter()).copied().collect();
    ordered.sort_unstable();
    ordered.dedup();
    let last = ordered.pop().expect("non-empty skip dirs");
    format!("{}, and {last}", ordered.join(", "))
}

pub fn help_text() -> String {
    format!(
        "Usage: dryer [options] [path-or-filter ...]

Find candidate duplicate code in Clojure, Java, Go, TypeScript, Vue, Rust,
Python, and Swift. Each form is normalized — call and method names, operators,
and tree shape stay; local names, field names, and literals do not — and
compared with the other forms of the same language. The score is Jaccard
similarity of those structural fingerprints, the same score as dry4clj.

Prints a report and writes .metrics/dry.edn.

Languages: Clojure (.clj .cljc .cljs .cljd .bb), Java (.java), Go (.go),
TypeScript (.ts .tsx .mts .cts), Vue (.vue), Rust (.rs), Python (.py),
Swift (.swift).

Options:
  -h, --help                    Print this help and exit.
  --root <path>                 Project root. Metrics are written here.
                                Default: the current directory.
  -s, --source-root <path>      Walk this tree instead of the project root.
                                May be repeated.
  --changed                     Compare added and modified source files from
                                git status.
  --focus-changed               Scan full target scope, but report only
                                duplicate pairs overlapping git-changed lines
                                (plus all forms in new files; implies
                                --no-metrics).
  --base <ref>                  Git base ref for --changed or --focus-changed
                                (compares <ref> against working tree).
  --threshold N                 Minimum similarity score, default 0.82.
  --min-lines N                 Minimum source lines in a candidate form,
                                default 4.
  --min-nodes N                 Minimum normalized syntax nodes, default 20.
  --format F                    text, edn, or json, default text.
  --edn                         Same as --format edn.
  --text                        Same as --format text.
  --json                        Same as --format json.
  --no-metrics                  Skip writing .metrics/dry.edn.
                                Print report only.
  --no-cache                    Skip reading and writing the AST fingerprint
                                cache.

Arguments:
  path              File or directory to compare. Test paths are included when
                    you name them explicitly.
  filter            When the argument is not a path, only source files whose
                    path contains this text are compared.

With no paths, source files under the project root are compared. Directories
named {} are skipped, as are *_test.go, *.test.cts,
*.spec.cts, *.spec.ts, *.test.vue, *.spec.vue, test_*.py, *_test.py, and
*Tests.swift files.

Clojure compares every top-level form except ns. The other languages compare
functions and methods. Swift also compares computed properties, including a
SwiftUI view body, a closure bound at file scope, and deinit. Vue `<script>`
blocks normalize and compare alongside TypeScript (functions, methods,
Options API hooks, and top-level Composition API callbacks). A form is only
compared with forms in the same language.

Exit codes:
  0  the report was written
  1  usage error
  2  unknown output format
",
        skipped_directory_text()
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Help,
    Format,
    Scan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub action: Action,
    pub message: String,
    pub exit_code: i32,
    pub project_root: PathBuf,
    pub source_roots: Vec<String>,
    pub positionals: Vec<String>,
    pub threshold: f64,
    pub min_lines: usize,
    pub min_nodes: usize,
    pub format: String,
    pub changed: bool,
    pub focus_changed: bool,
    pub base: Option<String>,
    pub no_metrics: bool,
    pub no_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            action: Action::Scan,
            message: String::new(),
            exit_code: 0,
            project_root: PathBuf::from("."),
            source_roots: Vec::new(),
            positionals: Vec::new(),
            threshold: 0.82,
            min_lines: 4,
            min_nodes: 20,
            format: "text".to_string(),
            changed: false,
            focus_changed: false,
            base: None,
            no_metrics: false,
            no_cache: false,
        }
    }
}

fn take_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    let Some(val) = args.get(index + 1) else {
        return Err(format!("{option} requires a value"));
    };
    if val.is_empty() || val.starts_with('-') {
        return Err(format!("{option} requires a value"));
    }
    Ok(val.as_str())
}

fn parse_number(value: &str, option: &str) -> Result<f64, String> {
    value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{option} requires a number"))
}

pub fn parse_count(value: &str, option: &str) -> Result<usize, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{option} requires an integer"));
    }
    let (negative, digits) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        (false, rest)
    } else {
        (false, trimmed)
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return Err(format!("{option} requires an integer"));
    }
    let all_zero = digits.chars().filter(|&c| c != '_').all(|c| c == '0');
    if negative && !all_zero {
        return Err(format!("{option} requires a non-negative integer"));
    }
    let mut acc: usize = 0;
    for ch in digits.chars() {
        if ch == '_' {
            continue;
        }
        let digit = (ch as u8 - b'0') as usize;
        acc = acc.saturating_mul(10).saturating_add(digit);
    }
    Ok(acc)
}

pub fn parse_args(args: &[String]) -> Options {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Options {
            action: Action::Help,
            message: help_text(),
            exit_code: 0,
            ..Options::default()
        };
    }

    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let step = match arg.as_str() {
            "-s" | "--source-root" => match take_value(args, index, arg) {
                Ok(v) => {
                    options.source_roots.push(v.to_string());
                    Ok(2)
                }
                Err(e) => Err(e),
            },
            "--root" => match take_value(args, index, arg) {
                Ok(v) => {
                    options.project_root = PathBuf::from(v);
                    Ok(2)
                }
                Err(e) => Err(e),
            },
            "--threshold" => take_value(args, index, arg)
                .and_then(|v| parse_number(v, arg))
                .map(|v| {
                    options.threshold = v;
                    2
                }),
            "--min-lines" => take_value(args, index, arg)
                .and_then(|v| parse_count(v, arg))
                .map(|v| {
                    options.min_lines = v;
                    2
                }),
            "--min-nodes" => take_value(args, index, arg)
                .and_then(|v| parse_count(v, arg))
                .map(|v| {
                    options.min_nodes = v;
                    2
                }),
            "--format" => take_value(args, index, arg).map(|v| {
                options.format = v.to_string();
                2
            }),
            "--edn" => {
                options.format = "edn".to_string();
                Ok(1)
            }
            "--text" => {
                options.format = "text".to_string();
                Ok(1)
            }
            "--json" => {
                options.format = "json".to_string();
                Ok(1)
            }
            "--changed" => {
                options.changed = true;
                Ok(1)
            }
            "--focus-changed" => {
                options.focus_changed = true;
                Ok(1)
            }
            "--base" => take_value(args, index, arg).map(|v| {
                options.base = Some(v.to_string());
                2
            }),
            "--no-metrics" => {
                options.no_metrics = true;
                Ok(1)
            }
            "--no-cache" => {
                options.no_cache = true;
                Ok(1)
            }
            _ if arg.starts_with('-') => Err(format!("Unknown option: {arg}")),
            _ => {
                options.positionals.push(arg.clone());
                Ok(1)
            }
        };
        match step {
            Ok(advance) => index += advance,
            Err(err) => {
                return Options {
                    action: Action::Help,
                    message: format!("{err}\n\n{}", help_text()),
                    exit_code: 1,
                    ..Options::default()
                };
            }
        }
    }

    if options.changed && options.focus_changed {
        return Options {
            action: Action::Help,
            message: format!(
                "--changed cannot be combined with --focus-changed\n\n{}",
                help_text()
            ),
            exit_code: 1,
            ..Options::default()
        };
    }

    if options.base.is_some() && !options.changed && !options.focus_changed {
        return Options {
            action: Action::Help,
            message: format!(
                "--base requires --changed or --focus-changed\n\n{}",
                help_text()
            ),
            exit_code: 1,
            ..Options::default()
        };
    }

    if !matches!(options.format.as_str(), "text" | "edn" | "json") {
        return Options {
            action: Action::Format,
            message: format!("Unknown format: {}\n", options.format),
            exit_code: 2,
            ..Options::default()
        };
    }

    if options.action == Action::Scan {
        if options.focus_changed || ub_runtime::env_no_metrics() {
            options.no_metrics = true;
        }
        if ub_runtime::env_no_cache() {
            options.no_cache = true;
        }
    }

    options
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusError {
    pub code: i32,
    pub message: String,
}

impl From<ub_runtime::GitCommandError> for GitStatusError {
    fn from(err: ub_runtime::GitCommandError) -> Self {
        Self {
            code: err.code,
            message: err.message,
        }
    }
}

fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, GitStatusError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| GitStatusError {
            code: 1,
            message: err.to_string(),
        })?;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(1);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            "git status failed".to_string()
        } else {
            stderr
        };
        return Err(GitStatusError { code, message });
    }
    Ok(output.stdout)
}

pub fn changed_files(root: &Path) -> Result<Vec<PathBuf>, GitStatusError> {
    let toplevel_bytes = run_git(root, &["rev-parse", "--show-toplevel"])?;
    let toplevel_str = String::from_utf8_lossy(&toplevel_bytes).trim().to_string();
    let toplevel = PathBuf::from(toplevel_str);
    let status = run_git(
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
    )?;
    let root_resolved = resolve_path(root);
    let mut found = Vec::new();
    for entry in status.split(|&b| b == 0) {
        if entry.len() <= 3 {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry[3..]);
        let path = resolve_path(&toplevel.join(rel.as_ref()));
        if !path.is_file() || !path.starts_with(&root_resolved) {
            continue;
        }
        found.push(path);
    }
    Ok(found)
}

pub fn tracked_source(path: &Path, root: Option<&Path>) -> bool {
    let resolved;
    let rel = if let Some(base) = root {
        resolved = resolve_path(path);
        let base_resolved = resolve_path(base);
        let Ok(stripped) = resolved.strip_prefix(&base_resolved) else {
            return false;
        };
        stripped
    } else {
        path
    };
    let in_skip_dir = rel.components().any(|component| match component {
        std::path::Component::Normal(part) => {
            part.to_str().is_some_and(|name| SKIP_DIRS.contains(&name))
        }
        _ => false,
    });
    if in_skip_dir {
        return false;
    }
    if language_of(path).is_none() {
        return false;
    }
    !is_test_file(path)
}

fn changed_source(root: &Path) -> Result<Vec<PathBuf>, GitStatusError> {
    let changed = changed_files(root)?;
    Ok(changed
        .into_iter()
        .filter(|path| tracked_source(path, Some(root)))
        .collect())
}

fn changed_source_with_base(
    root: &Path,
    base: Option<&str>,
) -> Result<Vec<PathBuf>, GitStatusError> {
    if base.is_none() {
        return changed_source(root);
    }
    let entries = ub_runtime::git_changed_lines(root, base)?;
    Ok(entries
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| tracked_source(path, Some(root)))
        .collect())
}

fn changed_source_lines(
    root: &Path,
    base: Option<&str>,
) -> Result<HashMap<String, ub_runtime::ChangedLines>, GitStatusError> {
    let root_resolved = resolve_path(root);
    let entries = ub_runtime::git_changed_lines(&root_resolved, base)?;
    let mut focused = HashMap::new();
    for (path, lines) in entries {
        if !tracked_source(&path, Some(&root_resolved)) {
            continue;
        }
        let resolved = resolve_path(&path);
        let rel = match resolved.strip_prefix(&root_resolved) {
            Ok(stripped) => to_posix(stripped),
            Err(_) => to_posix(&resolved),
        };
        focused.insert(rel, lines);
    }
    Ok(focused)
}

fn span_overlaps_changed(
    span: &super::model::Span,
    focused: &HashMap<String, ub_runtime::ChangedLines>,
) -> bool {
    focused
        .get(&span.file)
        .is_some_and(|lines| lines.overlaps(span.start_line, span.end_line))
}

fn write_focus_excluded_info(err: &mut dyn Write, excluded: usize, threshold: f64) {
    if excluded == 0 {
        return;
    }
    let pair_word = if excluded == 1 { "pair" } else { "pairs" };
    let _ = writeln!(
        err,
        "[dryer] INFO (non-blocking): {excluded} unchanged legacy duplicate {pair_word} >= {threshold:.2} excluded by --focus-changed."
    );
}

fn positionals(root: &Path, args: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut existing = Vec::new();
    let mut filters = Vec::new();
    for arg in args {
        let candidate = if Path::new(arg).is_absolute() {
            PathBuf::from(arg)
        } else {
            root.join(arg)
        };
        if candidate.exists() {
            existing.push(resolve_path(&candidate));
        } else {
            filters.push(arg.clone());
        }
    }
    (existing, filters)
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
) -> Result<Vec<PathBuf>, GitStatusError> {
    if options.changed {
        return changed_source_with_base(root, options.base.as_deref());
    }
    if !options.source_roots.is_empty() {
        let roots: Vec<PathBuf> = options
            .source_roots
            .iter()
            .map(|p| resolve_path(&root.join(p)))
            .collect();
        return Ok(iter_source_files(&roots));
    }
    if !existing.is_empty() {
        return Ok(explicit_files(existing));
    }
    Ok(iter_source_files(std::slice::from_ref(&root.to_path_buf())))
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

pub fn select_files(options: &Options) -> Result<Vec<PathBuf>, GitStatusError> {
    let root = resolve_path(&options.project_root);
    let (existing, filters) = positionals(&root, &options.positionals);
    let candidates = apply_filters(selected(options, &root, &existing)?, &filters);
    let mut unique = BTreeMap::new();
    for resolved in candidates {
        let key = to_posix(&resolved);
        unique.entry(key).or_insert(resolved);
    }
    Ok(unique.into_values().collect())
}

fn format_report(
    candidates: &[super::model::Duplicate],
    entries_scanned: usize,
    threshold: f64,
    format: &str,
) -> String {
    match format {
        "json" => format_json(candidates, entries_scanned, threshold),
        "edn" => render_edn(candidates),
        _ => format_text(candidates),
    }
}

fn write_dryer_summary(
    err: &mut dyn Write,
    candidates: &[super::model::Duplicate],
    entries_scanned: usize,
    threshold: f64,
) {
    if candidates.is_empty() {
        let _ = writeln!(
            err,
            "[dryer] PASS (exit 0): 0 duplicate pairs >= {threshold:.2} ({entries_scanned} forms scanned)."
        );
        return;
    }
    let count = candidates.len();
    let pair_word = if count == 1 { "pair" } else { "pairs" };
    if let Some(top) = candidates.iter().max_by(|a, b| a.score.total_cmp(&b.score)) {
        let _ = writeln!(
            err,
            "[dryer] FOUND (exit 0, review required): {count} duplicate {pair_word} >= {threshold:.2} ({entries_scanned} forms scanned; top: score={:.2} {}:{}-{} <-> {}:{}-{}). Exit 0 does not mean clean; inspect stdout candidates.",
            top.score,
            top.left.file,
            top.left.start_line,
            top.left.end_line,
            top.right.file,
            top.right.start_line,
            top.right.end_line,
        );
    }
}

pub fn run_with_writers(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let options = parse_args(args);
    match options.action {
        Action::Help => {
            if options.exit_code == 0 {
                let _ = write!(out, "{}", options.message);
                if !options.message.ends_with('\n') {
                    let _ = writeln!(out);
                }
            } else {
                let _ = write!(err, "{}", options.message);
                if !options.message.ends_with('\n') {
                    let _ = writeln!(err);
                }
            }
            return options.exit_code;
        }
        Action::Format => {
            let _ = write!(err, "{}", options.message);
            return options.exit_code;
        }
        Action::Scan => {}
    }

    let root = resolve_path(&options.project_root);
    let focused_lines = if options.focus_changed {
        let map = match changed_source_lines(&root, options.base.as_deref()) {
            Ok(map) => map,
            Err(git_err) => {
                let _ = writeln!(err, "{}", git_err.message);
                return git_err.code;
            }
        };
        if map.is_empty() {
            let _ = write!(
                out,
                "{}",
                format_report(&[], 0, options.threshold, &options.format)
            );
            let _ = out.flush();
            let _ = writeln!(
                err,
                "[dryer] PASS (exit 0): 0 changed source files to focus (0 duplicates)."
            );
            return 0;
        }
        Some(map)
    } else {
        None
    };

    let files = match select_files(&options) {
        Ok(files) => files,
        Err(git_err) => {
            let _ = writeln!(err, "{}", git_err.message);
            return git_err.code;
        }
    };

    if files.is_empty() {
        if options.format == "json" {
            let _ = write!(out, "{}", format_json(&[], 0, options.threshold));
        } else {
            let _ = writeln!(out, "No source files to analyze.");
        }
        let _ = out.flush();
        let _ = writeln!(
            err,
            "[dryer] PASS (exit 0): 0 source files matched (0 duplicates)."
        );
        return 0;
    }

    let _slot = ub_runtime::acquire_global_scan_slot().ok().flatten();

    let (entries_scanned, mut candidates, warnings) = scan_and_find_duplicates_resolved(
        &files,
        &root,
        options.min_lines,
        options.min_nodes,
        options.threshold,
        !options.no_cache,
    );
    for warning in warnings {
        let _ = writeln!(err, "[dryer] WARN: {warning}");
    }
    let mut excluded_by_focus = 0usize;
    if let Some(ref focused) = focused_lines {
        let before = candidates.len();
        candidates.retain(|dup| {
            span_overlaps_changed(&dup.left, focused) || span_overlaps_changed(&dup.right, focused)
        });
        excluded_by_focus = before - candidates.len();
    }
    let written_path = if options.no_metrics {
        None
    } else {
        match write_metrics(&candidates, &root) {
            Ok(metrics) => Some(metrics),
            Err(io_err) => {
                let _ = writeln!(err, "dryer: failed to write metrics: {io_err}");
                return 1;
            }
        }
    };
    let _ = write!(
        out,
        "{}",
        format_report(
            &candidates,
            entries_scanned,
            options.threshold,
            &options.format
        )
    );
    let _ = out.flush();
    if let Some(metrics) = written_path {
        let _ = writeln!(err, "Wrote {}", metrics.display());
    }
    write_focus_excluded_info(err, excluded_by_focus, options.threshold);
    write_dryer_summary(err, &candidates, entries_scanned, options.threshold);
    0
}

pub fn run(args: &[String]) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with_writers(args, &mut stdout, &mut stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn parses_options_and_rejects_bad_inputs() {
        let help = parse_args(&s(&["--help"]));
        assert_eq!(help.action, Action::Help);
        assert_eq!(help.exit_code, 0);
        assert!(help.message.contains("Usage: dryer"));
        assert!(help.message.contains("__pycache__"));
        assert!(help.message.contains("*Tests.swift"));

        let fmt = parse_args(&s(&["--format", "csv"]));
        assert_eq!(fmt.exit_code, 2);
        assert_eq!(fmt.message, "Unknown format: csv\n");

        let unknown = parse_args(&s(&["--nope"]));
        assert_eq!(unknown.exit_code, 1);
        assert!(unknown.message.contains("Unknown option: --nope"));

        let parsed = parse_args(&s(&[
            "--threshold",
            "0.9",
            "--min-lines",
            "5",
            "--min-nodes",
            "30",
            "--edn",
            "spec",
        ]));
        assert_eq!(parsed.threshold, 0.9);
        assert_eq!(parsed.min_lines, 5);
        assert_eq!(parsed.min_nodes, 30);
        assert_eq!(parsed.format, "edn");
        assert_eq!(parsed.positionals, vec!["spec"]);
    }

    #[test]
    fn runs_clojure_scan_end_to_end() {
        let dir = std::env::temp_dir().join(format!("dryer-cli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("left.cljc"),
            "(ns sample.left)\n\n(defn alpha [xs]\n  (let [ys (filter odd? xs)]\n    (map inc ys)))\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("right.cljc"),
            "(ns sample.right)\n\n(defn beta [items]\n  (let [kept (filter even? items)]\n    (map dec kept)))\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_writers(
            &s(&[
                "--root",
                dir.to_str().unwrap(),
                "--min-lines",
                "3",
                "--min-nodes",
                "8",
            ]),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let out_str = String::from_utf8(out).unwrap();
        assert!(out_str.contains("DUPLICATE score=1.00"));
        assert!(out_str.contains("left.cljc:3-5"));
        assert!(out_str.contains("right.cljc:3-5"));
        let edn = std::fs::read_to_string(dir.join(".metrics").join("dry.edn")).unwrap();
        assert!(edn.contains(":language \"clojure\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_files_reads_real_git_status_and_reports_failures() {
        let base = std::env::temp_dir().join(format!("dryer-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let non_repo = base.join("plain");
        std::fs::create_dir_all(&non_repo).unwrap();
        let err = changed_files(&non_repo).unwrap_err();
        assert_eq!(err.code, 128);
        assert!(err.message.to_lowercase().contains("not a git repository"));

        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert!(
            Command::new("git")
                .args(["init"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.email", "dev@example.com"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.name", "Dev"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(repo.join("src/a.py"), "x = 1\n").unwrap();
        std::fs::write(repo.join("src/old.py"), "x = 1\n").unwrap();
        std::fs::write(repo.join("gone.py"), "x = 1\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "-A"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "base"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );

        std::fs::remove_file(repo.join("gone.py")).unwrap();
        assert!(
            Command::new("git")
                .args(["mv", "src/old.py", "src/b.py"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(repo.join("src/c d.py"), "x = 1\n").unwrap();
        std::fs::write(repo.join("src/café.py"), "x = 1\n").unwrap();

        let found = changed_files(&repo).unwrap();
        assert!(found.contains(&resolve_path(&repo.join("src/b.py"))));
        assert!(found.contains(&resolve_path(&repo.join("src/c d.py"))));
        assert!(found.contains(&resolve_path(&repo.join("src/café.py"))));
        assert!(!found.contains(&resolve_path(&repo.join("gone.py"))));

        // Subdirectory isolation + --source-root + filter + format override
        let sub = repo.join("sub");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        std::fs::write(sub.join("src/below.py"), "x = 1\n").unwrap();
        let sub_opts = parse_args(&s(&["--root", sub.to_str().unwrap(), "--changed"]));
        let sub_selected = select_files(&sub_opts).unwrap();
        assert_eq!(sub_selected, vec![resolve_path(&sub.join("src/below.py"))]);

        let text_opts = parse_args(&s(&["--edn", "--text"]));
        assert_eq!(text_opts.format, "text");

        assert_eq!(
            parse_count("18446744073709551616", "--min-lines"),
            Ok(usize::MAX)
        );
        assert_eq!(parse_count("1_000", "--min-lines"), Ok(1000));
        assert_eq!(parse_count("-0", "--min-lines"), Ok(0));
        assert!(
            parse_count("-1", "--min-lines")
                .unwrap_err()
                .contains("non-negative")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn duplicate_clojure_fixture(label: &str) -> TempDirGuard {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("dryer-{label}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("left.cljc"),
            "(ns sample.left)\n\n(defn alpha [xs]\n  (let [ys (filter odd? xs)]\n    (map inc ys)))\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("right.cljc"),
            "(ns sample.right)\n\n(defn beta [items]\n  (let [kept (filter even? items)]\n    (map dec kept)))\n",
        )
        .unwrap();
        TempDirGuard(dir)
    }

    fn run_fixture_scan(dir: &Path, extra_args: &[&str]) -> (i32, String, String) {
        let mut args = vec![
            "--root".to_string(),
            dir.to_str().unwrap().to_string(),
            "--min-lines".to_string(),
            "3".to_string(),
            "--min-nodes".to_string(),
            "8".to_string(),
        ];
        args.extend(extra_args.iter().map(|a| (*a).to_string()));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_writers(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn no_metrics_flag_skips_writing_metrics_for_text_and_edn() {
        assert!(!Options::default().no_metrics);
        assert!(help_text().contains("--no-metrics"));
        assert!(parse_args(&s(&["--no-metrics"])).no_metrics);

        let dir = duplicate_clojure_fixture("no-metrics-flag");
        let metrics_file = dir.path().join(".metrics").join("dry.edn");

        let (code, text_out, text_err) = run_fixture_scan(dir.path(), &["--no-metrics"]);
        assert_eq!(code, 0);
        assert!(text_out.contains("DUPLICATE score=1.00"));
        assert!(text_out.contains("left.cljc:3-5"));
        assert!(text_out.contains("right.cljc:3-5"));
        assert!(!text_err.contains("Wrote "));
        assert!(!metrics_file.exists());
        assert!(!dir.path().join(".metrics").exists());

        let (edn_code, edn_out, edn_err) = run_fixture_scan(dir.path(), &["--no-metrics", "--edn"]);
        assert_eq!(edn_code, 0);
        assert!(edn_out.contains("{:candidates ["));
        assert!(edn_out.contains(":language \"clojure\""));
        assert!(!edn_err.contains("Wrote "));
        assert!(!metrics_file.exists());
        assert!(!dir.path().join(".metrics").exists());
    }

    const SUBPROCESS_MARKER: &str = "DRYER_CLI_NO_METRICS_SUBPROC";
    const SUBPROCESS_EXPECT_NO_METRICS: &str = "DRYER_CLI_EXPECT_NO_METRICS";

    fn run_no_metrics_subproc_case(env_val: Option<&str>, expect_no_metrics: bool) {
        let exe = std::env::current_exe().expect("current test exe");
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "cli::tests::env_ub_tools_no_metrics_skips_metrics_in_isolated_subprocess",
            "--nocapture",
        ]);
        cmd.env(SUBPROCESS_MARKER, "1");
        cmd.env(
            SUBPROCESS_EXPECT_NO_METRICS,
            if expect_no_metrics { "1" } else { "0" },
        );
        match env_val {
            Some(v) => {
                cmd.env("UB_TOOLS_NO_METRICS", v);
            }
            None => {
                cmd.env_remove("UB_TOOLS_NO_METRICS");
            }
        }
        let output = cmd.output().expect("spawn subprocess self-test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "subprocess test failed for UB_TOOLS_NO_METRICS={env_val:?}:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn env_ub_tools_no_metrics_skips_metrics_in_isolated_subprocess() {
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            let expect_no_metrics = std::env::var_os(SUBPROCESS_EXPECT_NO_METRICS).as_deref()
                == Some(std::ffi::OsStr::new("1"));
            assert_eq!(parse_args(&[]).no_metrics, expect_no_metrics);
            assert!(!parse_args(&s(&["--help"])).no_metrics);
            assert!(!parse_args(&s(&["--format", "csv"])).no_metrics);

            let dir = duplicate_clojure_fixture("env-subproc");
            let (code, out_str, err_str) = run_fixture_scan(dir.path(), &[]);
            assert_eq!(code, 0);
            assert!(out_str.contains("DUPLICATE score=1.00"));
            let metrics_file = dir.path().join(".metrics").join("dry.edn");
            if expect_no_metrics {
                assert!(!err_str.contains("Wrote "));
                assert!(!metrics_file.exists());
            } else {
                assert!(err_str.contains("Wrote "));
                assert!(metrics_file.is_file());
            }
            return;
        }

        run_no_metrics_subproc_case(Some("1"), true);
        run_no_metrics_subproc_case(Some(" true "), true);
        run_no_metrics_subproc_case(Some("0"), false);
        run_no_metrics_subproc_case(None, false);
    }

    #[test]
    fn parses_json_and_no_cache_options_and_keeps_exit_code_2_for_unknown_format() {
        assert!(!Options::default().no_cache);
        let help = help_text();
        assert!(help.contains("--json"));
        assert!(help.contains("--no-cache"));
        assert!(help.contains("json"));

        let json_flag = parse_args(&s(&["--json"]));
        assert_eq!(json_flag.action, Action::Scan);
        assert_eq!(json_flag.exit_code, 0);
        assert_eq!(json_flag.format, "json");

        let format_json = parse_args(&s(&["--format", "json", "--no-cache"]));
        assert_eq!(format_json.action, Action::Scan);
        assert_eq!(format_json.exit_code, 0);
        assert_eq!(format_json.format, "json");
        assert!(format_json.no_cache);

        let unknown_fmt = parse_args(&s(&["--format", "xml"]));
        assert_eq!(unknown_fmt.action, Action::Format);
        assert_eq!(unknown_fmt.exit_code, 2);
        assert_eq!(unknown_fmt.message, "Unknown format: xml\n");
    }

    #[test]
    fn json_output_handles_empty_files_and_candidate_matches() {
        let empty_dir = std::env::temp_dir().join(format!(
            "dryer-cli-empty-json-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let _empty_guard = TempDirGuard(empty_dir.clone());

        for flag in [&["--json", "--no-metrics"][..], &["--format", "json"][..]] {
            let mut args = vec![
                "--root".to_string(),
                empty_dir.to_str().unwrap().to_string(),
            ];
            args.extend(flag.iter().map(|x| (*x).to_string()));
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run_with_writers(&args, &mut out, &mut err);
            assert_eq!(code, 0);
            let empty_err_str = String::from_utf8(err).unwrap();
            assert!(empty_err_str.contains("[dryer] PASS (exit 0): 0 source files matched"));
            let out_str = String::from_utf8(out).unwrap();
            assert!(out_str.ends_with('\n'));
            let json: serde_json::Value = serde_json::from_str(&out_str).unwrap();
            assert_eq!(json["version"], 1);
            assert_eq!(json["tool"], "dryer");
            assert_eq!(json["threshold"], 0.82);
            assert_eq!(json["summary"]["entries_scanned"], 0);
            assert_eq!(json["summary"]["duplicates"], 0);
            assert_eq!(json["candidates"].as_array().unwrap().len(), 0);
        }

        let dir = duplicate_clojure_fixture("cli-json");
        let (code_no_metrics, out_no_metrics, err_no_metrics) =
            run_fixture_scan(dir.path(), &["--json", "--no-metrics", "--no-cache"]);
        assert_eq!(code_no_metrics, 0);
        assert!(!err_no_metrics.contains("Wrote "));
        assert!(
            err_no_metrics
                .contains("[dryer] FOUND (exit 0, review required): 1 duplicate pair >= 0.82")
        );
        assert!(err_no_metrics.contains("Exit 0 does not mean clean"));
        assert!(!dir.path().join(".metrics").exists());
        let val: serde_json::Value = serde_json::from_str(&out_no_metrics).unwrap();
        assert_eq!(val["version"], 1);
        assert_eq!(val["tool"], "dryer");
        assert_eq!(val["summary"]["entries_scanned"], 2);
        assert_eq!(val["summary"]["duplicates"], 1);
        assert_eq!(val["candidates"][0]["language"], "clojure");
        assert_eq!(val["candidates"][0]["score"], 1.0);

        let (code_with_metrics, out_with_metrics, err_with_metrics) =
            run_fixture_scan(dir.path(), &["--format", "json", "--no-cache"]);
        assert_eq!(code_with_metrics, 0);
        assert!(err_with_metrics.contains("Wrote "));
        assert!(
            err_with_metrics
                .contains("[dryer] FOUND (exit 0, review required): 1 duplicate pair >= 0.82")
        );
        assert!(dir.path().join(".metrics").join("dry.edn").is_file());
        let val2: serde_json::Value = serde_json::from_str(&out_with_metrics).unwrap();
        assert_eq!(val2, val);
    }

    const NO_CACHE_SUBPROC: &str = "DRYER_CLI_NO_CACHE_SUBPROC";
    const NO_CACHE_ENV_MODE: &str = "DRYER_CLI_NO_CACHE_MODE";

    fn run_no_cache_subproc_case(env_val: Option<&str>, mode: &str) {
        let cache_dir =
            std::env::temp_dir().join(format!("dryer-cli-nocache-{mode}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let _guard = TempDirGuard(cache_dir.clone());

        let exe = std::env::current_exe().expect("current test exe");
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "cli::tests::no_cache_flag_and_env_bypass_cache_in_isolated_subprocess",
            "--nocapture",
        ]);
        cmd.env(NO_CACHE_SUBPROC, "1");
        cmd.env(NO_CACHE_ENV_MODE, mode);
        cmd.env("UB_TOOLS_CACHE_DIR", &cache_dir);
        match env_val {
            Some(v) => {
                cmd.env("UB_TOOLS_NO_CACHE", v);
            }
            None => {
                cmd.env_remove("UB_TOOLS_NO_CACHE");
            }
        }
        let output = cmd.output().expect("spawn no-cache subprocess test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "no-cache subproc test failed for mode={mode}:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn no_cache_flag_and_env_bypass_cache_in_isolated_subprocess() {
        if std::env::var_os(NO_CACHE_SUBPROC).is_some() {
            let mode = std::env::var(NO_CACHE_ENV_MODE).unwrap();
            let dir = duplicate_clojure_fixture(&format!("nocache-{mode}"));
            let cache_dir = ub_runtime::cache_dir();

            match mode.as_str() {
                "env_1" => {
                    assert!(parse_args(&[]).no_cache);
                    assert!(!parse_args(&s(&["--help"])).no_cache);
                    assert!(!parse_args(&s(&["--format", "csv"])).no_cache);
                    let (code, out, _) = run_fixture_scan(dir.path(), &["--json", "--no-metrics"]);
                    assert_eq!(code, 0);
                    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
                    assert_eq!(parsed["summary"]["duplicates"], 1);
                    assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 0);
                }
                "cli_flag" => {
                    assert!(!parse_args(&[]).no_cache);
                    assert!(parse_args(&s(&["--no-cache"])).no_cache);
                    let (code, out, _) =
                        run_fixture_scan(dir.path(), &["--json", "--no-metrics", "--no-cache"]);
                    assert_eq!(code, 0);
                    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
                    assert_eq!(parsed["summary"]["duplicates"], 1);
                    assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 0);
                }
                "warm_cli" => {
                    let (code1, out1, _) =
                        run_fixture_scan(dir.path(), &["--json", "--no-metrics"]);
                    assert_eq!(code1, 0);
                    let cache_files: Vec<PathBuf> = std::fs::read_dir(&cache_dir)
                        .unwrap()
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .collect();
                    assert_eq!(cache_files.len(), 1);
                    let stamp = ub_runtime::file_mtime_size(&cache_files[0]).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));

                    let (code2, out2, _) =
                        run_fixture_scan(dir.path(), &["--json", "--no-metrics"]);
                    assert_eq!(code2, 0);
                    assert_eq!(out1, out2);
                    assert_eq!(ub_runtime::file_mtime_size(&cache_files[0]).unwrap(), stamp);
                }
                other => panic!("unexpected mode: {other}"),
            }
            return;
        }

        run_no_cache_subproc_case(Some("1"), "env_1");
        run_no_cache_subproc_case(None, "cli_flag");
        run_no_cache_subproc_case(None, "warm_cli");
    }

    #[test]
    fn focus_changed_and_base_options_validate_and_filter_untouched_legacy_duplicates() {
        let help = help_text();
        assert!(help.contains("--focus-changed"));
        assert!(help.contains("--base <ref>"));

        let focus = parse_args(&s(&["--focus-changed"]));
        assert_eq!(focus.action, Action::Scan);
        assert!(focus.focus_changed);
        assert!(focus.no_metrics);

        let base_alone = parse_args(&s(&["--base", "main"]));
        assert_eq!(base_alone.exit_code, 1);
        assert!(
            base_alone
                .message
                .contains("--base requires --changed or --focus-changed")
        );

        let both = parse_args(&s(&["--changed", "--focus-changed"]));
        assert_eq!(both.exit_code, 1);
        assert!(
            both.message
                .contains("--changed cannot be combined with --focus-changed")
        );

        let base = std::env::temp_dir().join(format!("dryer-focus-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let guard = TempDirGuard(base.clone());
        let repo = &guard.0;
        std::fs::create_dir_all(repo.join("src")).unwrap();

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "dev@example.com"]);
        git(&["config", "user.name", "Dev"]);

        let legacy_body = "def legacy_dup(x, y):\n    total = x + y\n    if total > 5:\n        return total * 2\n    return total\n";
        let shared_body = "def existing_helper(a, b):\n    summed = a + b\n    if summed > 10:\n        return summed + 3\n    return summed\n";
        std::fs::write(repo.join("src/legacy_a.py"), legacy_body).unwrap();
        std::fs::write(
            repo.join("src/legacy_b.py"),
            legacy_body.replace("legacy_dup", "legacy_dup_two"),
        )
        .unwrap();
        std::fs::write(repo.join("src/base.py"), shared_body).unwrap();
        std::fs::write(repo.join("src/clean.py"), "def ok():\n    return 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", "initial"]);

        // Add an untracked file copying unchanged `src/base.py`.
        std::fs::write(
            repo.join("src/new_copy.py"),
            shared_body.replace("existing_helper", "copied_helper"),
        )
        .unwrap();

        // Plain `--changed` misses cross-file duplication against unchanged `src/base.py`.
        let (changed_code, changed_out, _) =
            run_fixture_scan(repo, &["--changed", "--json", "--no-metrics"]);
        assert_eq!(changed_code, 0);
        let changed_val: serde_json::Value = serde_json::from_str(&changed_out).unwrap();
        assert_eq!(changed_val["summary"]["duplicates"], 0);

        // `--focus-changed` finds `(src/base.py, src/new_copy.py)`, excludes the untouched legacy pair,
        // and implicitly skips writing `.metrics/dry.edn`.
        let (focus_code, focus_out, focus_err) =
            run_fixture_scan(repo, &["--focus-changed", "--json"]);
        assert_eq!(focus_code, 0);
        assert!(!repo.join(".metrics").exists());
        assert!(
            focus_err.contains("[dryer] INFO (non-blocking): 1 unchanged legacy duplicate pair"),
            "stderr was: {focus_err}"
        );
        assert!(
            focus_err.contains("[dryer] FOUND (exit 0, review required): 1 duplicate pair"),
            "stderr was: {focus_err}"
        );
        let info_pos = focus_err
            .find("[dryer] INFO (non-blocking): 1 unchanged legacy duplicate pair")
            .unwrap();
        let found_pos = focus_err
            .find("[dryer] FOUND (exit 0, review required): 1 duplicate pair")
            .unwrap();
        assert!(
            info_pos < found_pos,
            "expected INFO before FOUND in stderr:\n{focus_err}"
        );
        let focus_val: serde_json::Value = serde_json::from_str(&focus_out).unwrap();
        assert_eq!(focus_val["summary"]["duplicates"], 1);
        let pair = &focus_val["candidates"][0];
        let files = [
            pair["left"]["file"].as_str().unwrap(),
            pair["right"]["file"].as_str().unwrap(),
        ];
        assert!(files.contains(&"src/base.py"));
        assert!(files.contains(&"src/new_copy.py"));

        // Commit `src/new_copy.py` and verify `--focus-changed --base HEAD~1` still isolates that pair.
        git(&["add", "-A"]);
        git(&["commit", "-m", "add copy"]);
        let (base_code, base_out, base_err) =
            run_fixture_scan(repo, &["--focus-changed", "--base", "HEAD~1", "--json"]);
        assert_eq!(base_code, 0);
        assert!(
            base_err.contains("[dryer] INFO (non-blocking): 1 unchanged legacy duplicate pair")
        );
        let base_val: serde_json::Value = serde_json::from_str(&base_out).unwrap();
        assert_eq!(base_val["summary"]["duplicates"], 1);
    }

    #[test]
    fn focus_changed_filters_at_line_granularity_inside_modified_files() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("dryer-focus-lines-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let guard = TempDirGuard(base);
        let repo = &guard.0;
        std::fs::create_dir_all(repo.join("src")).unwrap();

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "dev@example.com"]);
        git(&["config", "user.name", "Dev"]);

        let legacy_a = "\
def legacy_fn_a(x, y):
    total = x + y
    if total > 5:
        return total * 2
    return total
";
        let legacy_b = "\
def legacy_fn_b(p, q):
    summed = p + q
    if summed > 9:
        return summed * 3
    return summed
";
        let unrelated = "\
def unrelated_fn():
    return 1
";
        let shared_helper = "\
def shared_helper(items, limit):
    selected = []
    for item in items:
        if item > limit:
            selected.append(item)
    return selected
";
        let copied_helper = "\
def copied_helper(entries, cutoff):
    chosen = []
    for entry in entries:
        if entry > cutoff:
            chosen.append(entry)
    return chosen
";

        std::fs::write(repo.join("src/legacy_other.py"), legacy_a).unwrap();
        let mixed_initial = format!("{legacy_b}\n{unrelated}");
        std::fs::write(repo.join("src/mixed.py"), &mixed_initial).unwrap();
        std::fs::write(repo.join("src/base.py"), shared_helper).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", "initial"]);

        let assert_stderr_order = |stderr: &str, excluded_msg: &str, verdict_msg: &str| {
            let info_pos = stderr
                .find(excluded_msg)
                .unwrap_or_else(|| panic!("missing `{excluded_msg}` in stderr:\n{stderr}"));
            let verdict_pos = stderr
                .find(verdict_msg)
                .unwrap_or_else(|| panic!("missing `{verdict_msg}` in stderr:\n{stderr}"));
            assert!(
                info_pos < verdict_pos,
                "expected `{excluded_msg}` before `{verdict_msg}`:\n{stderr}"
            );
        };

        // Append `copied_helper` to `src/mixed.py` without touching `legacy_fn_b`.
        let mixed_with_copy = format!("{mixed_initial}\n{copied_helper}");
        std::fs::write(repo.join("src/mixed.py"), &mixed_with_copy).unwrap();

        let (wt_code, wt_out, wt_err) = run_fixture_scan(repo, &["--focus-changed", "--json"]);
        assert_eq!(wt_code, 0);
        let wt_val: serde_json::Value = serde_json::from_str(&wt_out).unwrap();
        assert_eq!(wt_val["summary"]["duplicates"], 1);
        let pair = &wt_val["candidates"][0];
        let mixed_side = if pair["left"]["file"] == "src/mixed.py" {
            &pair["left"]
        } else {
            &pair["right"]
        };
        let other_side = if pair["left"]["file"] == "src/mixed.py" {
            &pair["right"]
        } else {
            &pair["left"]
        };
        assert_eq!(mixed_side["file"], "src/mixed.py");
        assert_eq!(other_side["file"], "src/base.py");
        assert!(
            mixed_side["start_line"].as_u64().unwrap() > 8,
            "expected copied_helper span after unrelated_fn, got {mixed_side}"
        );
        assert_stderr_order(
            &wt_err,
            "[dryer] INFO (non-blocking): 1 unchanged legacy duplicate pair",
            "[dryer] FOUND (exit 0, review required): 1 duplicate pair",
        );

        // Commit `src/mixed.py` and verify `--focus-changed --base HEAD~1` preserves the result.
        git(&["add", "-A"]);
        git(&["commit", "-m", "add copied helper in mixed"]);
        let (base_code, base_out, base_err) =
            run_fixture_scan(repo, &["--focus-changed", "--base", "HEAD~1", "--json"]);
        assert_eq!(base_code, 0);
        let base_val: serde_json::Value = serde_json::from_str(&base_out).unwrap();
        assert_eq!(base_val["summary"]["duplicates"], 1);
        assert_stderr_order(
            &base_err,
            "[dryer] INFO (non-blocking): 1 unchanged legacy duplicate pair",
            "[dryer] FOUND (exit 0, review required): 1 duplicate pair",
        );

        // Edit only `unrelated_fn` so neither duplicate pair overlaps the touched lines.
        let mixed_edited_unrelated = mixed_with_copy.replace("return 1", "return 42");
        std::fs::write(repo.join("src/mixed.py"), &mixed_edited_unrelated).unwrap();
        let (clean_code, clean_out, clean_err) =
            run_fixture_scan(repo, &["--focus-changed", "--json"]);
        assert_eq!(clean_code, 0);
        let clean_val: serde_json::Value = serde_json::from_str(&clean_out).unwrap();
        assert_eq!(clean_val["summary"]["duplicates"], 0);
        assert_stderr_order(
            &clean_err,
            "[dryer] INFO (non-blocking): 2 unchanged legacy duplicate pairs",
            "[dryer] PASS (exit 0): 0 duplicate pairs",
        );
    }
}
