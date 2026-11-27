//! Command line for the multi-language mutation tool.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{BufWriter, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::OnceLock;

use rayon::prelude::*;
use regex::Regex;
use ub_runtime::{FileStamp, ManifestStampCache};

use crate::cache::{CachedSites, SiteCache, file_stamp, repo_cache_path};
use crate::engine::{
    BaselineCache, EngineProvider, MutateOptions, ScanOptions, mutate_file_cached_with_context,
    restore_backups, scan_file_cached_with_history, scan_file_resolved_with_history,
};
use crate::metrics::{HistoryIndex, source_key_resolved};
use crate::model::{FormResult, History, MutateSite, RunResult, ScanFileData, Site, SkippedFile};
use crate::path_util::{resolve_path, to_posix};
use crate::report::{
    MutateReport, format_mutate_json, format_results, format_scan, format_scan_json,
    format_site_log,
};
use crate::runner::CommandRunner;
use crate::sources::{DiscoverProvider, is_mutable_source, is_skipped_test, iter_source_files};
use crate::workers::WorkerContext;

pub fn comma_and(names: &[&str]) -> String {
    let mut ordered: Vec<&str> = names.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.len() == 1 {
        return ordered[0].to_string();
    }
    let last = ordered[ordered.len() - 1];
    format!("{}, and {last}", ordered[..ordered.len() - 1].join(", "))
}

/// Build the exact `--help` text matching `mutator.cli.HELP`.
pub fn help_text(skip_dirs: &[&str], test_dirs: &[&str]) -> String {
    let skip = comma_and(skip_dirs);
    let test = comma_and(test_dirs);
    format!(
        "\
Usage: mutator [options] [path-or-filter ...]

Discover mutation sites in Clojure, Java, Go, TypeScript, Rust, Python, and Swift,
run the project's tests against each one, and write `.metrics/mutate` for
uml-viewer. Namespaces and function names are the ones crapper writes into
`.metrics/crap.edn`.

A form id is `defn/<name>` or, for a private operation, `defn-/<name>`.
uml-viewer joins that name to the operation and joins `:namespace` to the
class.

Options:
  -h, --help                    Print this help and exit.
  --root <path>                 Project root. Metrics are written here.
                                Default: the current directory.
  -s, --source-root <path>      Walk this tree instead of the project root.
                                May be repeated.
  --changed                     Mutate added and modified source files from
                                git status.
  --scan                        List mutation sites. Do not run tests or
                                write metrics.
  --mutate-all                  Run every covered site, including ones the
                                last snapshot already killed.
  --since-last-run              Run survivors and sites in new or rewritten
                                functions. This is the default when a snapshot
                                exists.
  --lines <n,n,...>             Run only mutations on these source lines.
  --no-coverage                 Treat every site as covered.
  --use-existing-coverage       Read coverage already on disk. Same as
                                --reuse-coverage.
  --reuse-coverage              Read coverage already on disk.
  --coverage-command <cmd>      Run this command instead of the per-language
                                coverage tools, then read the reports it wrote.
  --test-command <cmd>          Run this command for the baseline and every
                                mutant. The working directory is the project
                                root.
  --xcode-scheme <name>         Scheme for an Xcode or iOS test run. Default:
                                the first shared scheme, or the project name.
  --xcode-destination <dest>    xcodebuild -destination value. iOS needs this,
                                and a Mac with Xcode. Example:
                                platform=iOS Simulator,name=iPhone 16.
  --timeout-factor <number>     Mutant timeout, as a multiple of the baseline
                                duration. Default: 10. The timeout is at least
                                2 seconds.
  --mutation-warning <number>   Warn when a file selects more covered sites
                                than this. Default: 50.
  --max-workers <number>        Run at most this many mutants of one file at
                                once. Default: one per core. The run uses the
                                smaller of this limit, the cores, and the
                                number of selected sites.
  --verbose                     Print each test command and each mutant.
  --json                        Emit machine-readable JSON output.
  --no-cache                    Bypass and do not update the site cache.

Arguments:
  path              File or directory to mutate. Test paths are skipped.
  filter            When the argument is not a path, only source files whose
                    path contains this text are mutated.

With no paths, source files under the project root are mutated. Directories
named {skip} are skipped at any depth. A test directory
({test}) is skipped when it is the first component of the
path, and so is src/test and src/tests. A deeper directory with one of those
names, such as src/pkg/test, is mutated. Files whose names mark them as tests
are also skipped. Swift also skips a directory whose name ends with Tests
(Tests, AppTests) at any depth, and a file named *Tests.swift.

The default, once a snapshot exists, reruns survivors and sites in functions
whose text changed. Killed mutants in unchanged functions are kept.

Selected mutants of one file run at the same time, one worker per core unless
--max-workers says otherwise. A worker is a symlink overlay under
target/mutation-workers with its own copy of the mutated file. The project
tree is left unchanged. A copy left under target/mutator-backup/ by an
interrupted older run is restored before a non-scan run.

Exit codes:
  0  every executed mutant was killed, or there was nothing to run
  1  usage error
  2  baseline tests failed
  3  at least one mutant survived

Coverage, when it is produced, uses the same commands as crapper:
  Clojure      clj -M:cov --lcov
  Java         Maven JaCoCo
  Go           go test ./... -coverprofile=...
  TypeScript   npm run coverage, Vitest, or c8
  Rust         cargo llvm-cov or cargo tarpaulin
  Python       coverage.py LCOV
  Swift        swift test --enable-code-coverage, then llvm-cov JSON;
               or xcodebuild test and xcrun xccov view --report --json

SwiftPM tests are `swift test`. Xcode and iOS tests are `xcodebuild test`.
iOS runs only on a Mac with Xcode.

A site on a line the report does not mark as hit is uncovered and is not run.
"
    )
}

/// Default fallback SKIP_DIRS matching `mutator.cli._skip_directory_text`.
pub const FALLBACK_SKIP_DIRS: &[&str] = &[
    ".clj-kondo",
    ".git",
    ".hg",
    ".idea",
    ".metrics",
    ".svn",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
    "testdata",
    "venv",
    "vendor",
];

/// Default fallback TEST_DIRS matching `mutator.cli._test_directory_text`.
pub const FALLBACK_TEST_DIRS: &[&str] = &["test", "tests", "spec", "specs", "__tests__"];

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub action: String,
    pub message: String,
    pub exit_code: i32,
    pub project_root: PathBuf,
    pub source_roots: Vec<String>,
    pub positionals: Vec<String>,
    pub scan: bool,
    pub mutate_all: bool,
    pub since_last_run: bool,
    pub lines: Option<HashSet<usize>>,
    pub no_coverage: bool,
    pub use_existing_coverage: bool,
    pub coverage_command: Option<String>,
    pub test_command: Option<String>,
    pub xcode_scheme: Option<String>,
    pub xcode_destination: Option<String>,
    pub timeout_factor: f64,
    pub mutation_warning: i64,
    pub max_workers: Option<usize>,
    pub changed: bool,
    pub verbose: bool,
    pub json: bool,
    pub no_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            action: "mutate".to_string(),
            message: String::new(),
            exit_code: 0,
            project_root: PathBuf::from("."),
            source_roots: Vec::new(),
            positionals: Vec::new(),
            scan: false,
            mutate_all: false,
            since_last_run: false,
            lines: None,
            no_coverage: false,
            use_existing_coverage: false,
            coverage_command: None,
            test_command: None,
            xcode_scheme: None,
            xcode_destination: None,
            timeout_factor: 10.0,
            mutation_warning: 50,
            max_workers: None,
            changed: false,
            verbose: false,
            json: false,
            no_cache: false,
        }
    }
}

fn take<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    if index + 1 >= args.len() || args[index + 1].is_empty() || args[index + 1].starts_with('-') {
        return Err(format!("{option} requires a value"));
    }
    Ok(args[index + 1].as_str())
}

fn is_python_printable(ch: char) -> bool {
    if ch == ' ' {
        return true;
    }
    if ch.is_ascii() {
        return matches!(ch, ' '..='~');
    }
    static NON_CONTROL_SEP: OnceLock<Regex> = OnceLock::new();
    let re = NON_CONTROL_SEP.get_or_init(|| Regex::new(r"^[^\p{C}\p{Z}]$").expect("valid regex"));
    let mut buf = [0u8; 4];
    re.is_match(ch.encode_utf8(&mut buf))
}

fn python_repr_str(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_python_printable(c) => out.push(c),
            c => {
                let cp = u32::from(c);
                if cp < 0x100 {
                    let _ = write!(out, "\\x{cp:02x}");
                } else if cp < 0x10000 {
                    let _ = write!(out, "\\u{cp:04x}");
                } else {
                    let _ = write!(out, "\\U{cp:08x}");
                }
            }
        }
    }
    out.push(quote);
    out
}

enum PythonInt {
    Valid(i128),
    PositiveOverflow,
    NegativeOverflow,
}

fn valid_python_digits(digits: &str) -> bool {
    !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
}

fn parse_python_int(raw: &str) -> Result<PythonInt, ()> {
    let trimmed = raw.trim();
    let (negative, rest) = if let Some(s) = trimmed.strip_prefix('-') {
        (true, s)
    } else if let Some(s) = trimmed.strip_prefix('+') {
        (false, s)
    } else {
        (false, trimmed)
    };
    if !valid_python_digits(rest) {
        return Err(());
    }
    let clean: String = rest.chars().filter(|&c| c != '_').collect();
    if clean.chars().all(|c| c == '0') {
        return Ok(PythonInt::Valid(0));
    }
    let signed = if negative { format!("-{clean}") } else { clean };
    match signed.parse::<i128>() {
        Ok(n) => Ok(PythonInt::Valid(n)),
        Err(_) if negative => Ok(PythonInt::NegativeOverflow),
        Err(_) => Ok(PythonInt::PositiveOverflow),
    }
}

fn parse_workers(value: &str) -> Result<usize, String> {
    match parse_python_int(value) {
        Ok(PythonInt::Valid(count)) if count >= 1 => {
            Ok(usize::try_from(count).unwrap_or(usize::MAX))
        }
        Ok(PythonInt::PositiveOverflow) => Ok(usize::MAX),
        _ => Err("--max-workers requires a positive integer".to_string()),
    }
}

fn parse_lines(value: &str) -> Result<HashSet<usize>, String> {
    let mut found = HashSet::new();
    for piece in value.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let line = match parse_python_int(piece) {
            Ok(PythonInt::Valid(n)) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
            Ok(PythonInt::PositiveOverflow) => usize::MAX,
            Ok(PythonInt::Valid(_)) | Ok(PythonInt::NegativeOverflow) => {
                return Err("--lines requires positive line numbers".to_string());
            }
            Err(()) => {
                return Err(format!(
                    "invalid literal for int() with base 10: {}",
                    python_repr_str(piece)
                ));
            }
        };
        found.insert(line);
    }
    if found.is_empty() {
        return Err("--lines requires a line number".to_string());
    }
    Ok(found)
}

fn help_error(msg: impl AsRef<str>, help: &str) -> Options {
    Options {
        action: "help".to_string(),
        message: format!("{}\n\n{help}", msg.as_ref()),
        exit_code: 1,
        ..Default::default()
    }
}

/// Parse CLI arguments using exact Python `mutator.cli.parse_args` rules.
pub fn parse_args_with_help(args: &[String], help: &str) -> Options {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Options {
            action: "help".to_string(),
            message: help.to_string(),
            exit_code: 0,
            ..Default::default()
        };
    }

    let mut options = Options::default();
    let mut index = 0usize;
    let parsed: Result<(), String> = (|| {
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "-s" | "--source-root" => {
                    options
                        .source_roots
                        .push(take(args, index, arg)?.to_string());
                    index += 2;
                }
                "--root" => {
                    options.project_root = PathBuf::from(take(args, index, arg)?);
                    index += 2;
                }
                "--coverage-command" => {
                    options.coverage_command = Some(take(args, index, arg)?.to_string());
                    index += 2;
                }
                "--test-command" => {
                    options.test_command = Some(take(args, index, arg)?.to_string());
                    index += 2;
                }
                "--xcode-scheme" => {
                    options.xcode_scheme = Some(take(args, index, arg)?.to_string());
                    index += 2;
                }
                "--xcode-destination" => {
                    options.xcode_destination = Some(take(args, index, arg)?.to_string());
                    index += 2;
                }
                "--timeout-factor" => {
                    let raw = take(args, index, arg)?;
                    let val = raw.parse::<f64>().map_err(|_| {
                        format!(
                            "could not convert string to float: {}",
                            python_repr_str(raw)
                        )
                    })?;
                    if val.is_nan() || val <= 0.0 {
                        return Err("--timeout-factor requires a positive number".to_string());
                    }
                    options.timeout_factor = val;
                    index += 2;
                }
                "--mutation-warning" => {
                    let raw = take(args, index, arg)?;
                    options.mutation_warning = match parse_python_int(raw) {
                        Ok(PythonInt::Valid(n)) => {
                            n.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
                        }
                        Ok(PythonInt::PositiveOverflow) => i64::MAX,
                        Ok(PythonInt::NegativeOverflow) => i64::MIN,
                        Err(()) => {
                            return Err(format!(
                                "invalid literal for int() with base 10: {}",
                                python_repr_str(raw)
                            ));
                        }
                    };
                    index += 2;
                }
                "--max-workers" => {
                    options.max_workers = Some(parse_workers(take(args, index, arg)?)?);
                    index += 2;
                }
                "--lines" => {
                    options.lines = Some(parse_lines(take(args, index, arg)?)?);
                    index += 2;
                }
                "--no-coverage" => {
                    options.no_coverage = true;
                    index += 1;
                }
                "--use-existing-coverage" | "--reuse-coverage" => {
                    options.use_existing_coverage = true;
                    index += 1;
                }
                "--changed" => {
                    options.changed = true;
                    index += 1;
                }
                "--scan" => {
                    options.scan = true;
                    index += 1;
                }
                "--mutate-all" => {
                    options.mutate_all = true;
                    index += 1;
                }
                "--since-last-run" => {
                    options.since_last_run = true;
                    index += 1;
                }
                "--verbose" => {
                    options.verbose = true;
                    index += 1;
                }
                "--json" => {
                    options.json = true;
                    index += 1;
                }
                "--no-cache" => {
                    options.no_cache = true;
                    index += 1;
                }
                _ if arg.starts_with('-') => {
                    return Err(format!("Unknown option: {arg}"));
                }
                _ => {
                    options.positionals.push(arg.to_string());
                    index += 1;
                }
            }
        }
        Ok(())
    })();

    if let Err(err) = parsed {
        return help_error(err, help);
    }
    if options.mutate_all && options.since_last_run {
        return help_error(
            "--mutate-all cannot be combined with --since-last-run",
            help,
        );
    }
    if options.no_coverage && options.coverage_command.is_some() {
        return help_error(
            "--no-coverage cannot be combined with --coverage-command",
            help,
        );
    }
    if options.scan && options.mutate_all {
        return help_error("--scan cannot be combined with --mutate-all", help);
    }
    if options.scan && options.max_workers.is_some() {
        return help_error("--scan cannot be combined with --max-workers", help);
    }
    options.no_cache = options.no_cache || ub_runtime::env_no_cache();
    options
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusError {
    pub code: i32,
    pub message: String,
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, GitStatusError> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| GitStatusError {
            code: 128,
            message: e.to_string(),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            "git status failed".to_string()
        } else {
            stderr
        };
        return Err(GitStatusError {
            code: output.status.code().unwrap_or(1),
            message,
        });
    }
    Ok(output.stdout)
}

/// Added and modified files under `root`, resolved from `git status -z`.
pub fn changed_files(root: &Path) -> Result<Vec<PathBuf>, GitStatusError> {
    let top_bytes = git_stdout(root, &["rev-parse", "--show-toplevel"])?;
    let toplevel = PathBuf::from(String::from_utf8_lossy(&top_bytes).trim());
    let status = git_stdout(
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
        let candidate = resolve_path(&toplevel.join(rel.as_ref()));
        if !candidate.is_file() || !candidate.starts_with(&root_resolved) {
            continue;
        }
        found.push(candidate);
    }
    Ok(found)
}

fn split_positionals(root: &Path, args: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut existing = Vec::new();
    let mut filters = Vec::new();
    for arg in args {
        let p = Path::new(arg);
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        if candidate.exists() {
            existing.push(resolve_path(&candidate));
        } else {
            filters.push(arg.clone());
        }
    }
    (existing, filters)
}

fn explicit_sources<D: DiscoverProvider>(
    existing: &[PathBuf],
    root: &Path,
    discover: &D,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in existing {
        if path.is_dir() {
            files.extend(iter_source_files(
                std::slice::from_ref(path),
                root,
                discover,
            ));
            continue;
        }
        if is_mutable_source(path, root, discover) {
            files.push(path.clone());
            continue;
        }
        if is_skipped_test(path, root, discover) {
            eprintln!("Skipping test file {}", path.display());
        }
    }
    files
}

fn choose_files<D: DiscoverProvider>(
    options: &Options,
    root: &Path,
    existing: &[PathBuf],
    discover: &D,
) -> Result<Vec<PathBuf>, GitStatusError> {
    if options.changed {
        let changed = changed_files(root)?;
        return Ok(changed
            .into_iter()
            .filter(|p| is_mutable_source(p, root, discover))
            .collect());
    }
    if !options.source_roots.is_empty() {
        let starts: Vec<PathBuf> = options
            .source_roots
            .iter()
            .map(|p| resolve_path(&root.join(p)))
            .collect();
        return Ok(iter_source_files(&starts, root, discover));
    }
    if !existing.is_empty() {
        return Ok(explicit_sources(existing, root, discover));
    }
    Ok(iter_source_files(
        std::slice::from_ref(&root.to_path_buf()),
        root,
        discover,
    ))
}

fn matching_filters(files: Vec<PathBuf>, filters: &[String]) -> Vec<PathBuf> {
    if filters.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter(|path| {
            let text = to_posix(path);
            filters.iter().any(|item| text.contains(item.as_str()))
        })
        .collect()
}

fn inside_root(files: Vec<PathBuf>, root: &Path) -> Vec<PathBuf> {
    let mut inside = Vec::with_capacity(files.len());
    for path in files {
        if path.strip_prefix(root).is_ok() {
            inside.push(path);
            continue;
        }
        let resolved = resolve_path(&path);
        if resolved.strip_prefix(root).is_err() {
            eprintln!(
                "Skipping {}; it is outside {}",
                path.display(),
                root.display()
            );
            continue;
        }
        inside.push(resolved);
    }
    inside
}

/// Select sorted mutable source files under `options.project_root`.
pub fn select_files_with<D: DiscoverProvider>(
    options: &Options,
    discover: &D,
) -> Result<Vec<PathBuf>, GitStatusError> {
    let root = resolve_path(&options.project_root);
    let (existing, filters) = split_positionals(&root, &options.positionals);
    let files = choose_files(options, &root, &existing, discover)?;
    let matched = matching_filters(files, &filters);
    let inside = inside_root(matched, &root);
    let mut deduped: Vec<(String, PathBuf)> =
        inside.into_iter().map(|p| (to_posix(&p), p)).collect();
    deduped.sort_by(|a, b| a.0.cmp(&b.0));
    deduped.dedup_by(|a, b| a.0 == b.0);
    Ok(deduped.into_iter().map(|(_, p)| p).collect())
}

/// Abstraction for CLI execution dependencies (`crapper` coverage + engine).
pub trait CliProvider: DiscoverProvider + EngineProvider {
    fn run_coverage(&self, root: &Path, files: &[PathBuf]) -> i32;
    fn covered_lines(&self, root: &Path, path: &Path, language: &str) -> Option<HashSet<usize>>;
}

fn prepare_coverage<P: CliProvider>(
    options: &Options,
    root: &Path,
    files: &[PathBuf],
    provider: &P,
) -> i32 {
    if options.no_coverage || options.use_existing_coverage || options.scan {
        return 0;
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
    if let Some(cmd_str) = &options.coverage_command {
        use crate::runner::{Command, Runner};
        let result =
            CommandRunner::new(options.verbose).run(&Command::Shell(cmd_str.clone()), root, None);
        if result.code != 0 {
            eprintln!(
                "Coverage command exited {}. Reports already on disk will not be read.",
                result.code
            );
            let lines: Vec<&str> = result.output.lines().collect();
            let start = lines.len().saturating_sub(20);
            let tail = lines[start..].join("\n");
            if !tail.is_empty() {
                eprintln!("{tail}");
            }
            return result.code;
        }
        return 0;
    }
    provider.run_coverage(root, files)
}

fn coverage_for<P: CliProvider>(
    options: &Options,
    root: &Path,
    path: &Path,
    provider: &P,
) -> Option<HashSet<usize>> {
    if options.no_coverage {
        return None;
    }
    let language = DiscoverProvider::language_of(provider, path)?;
    provider.covered_lines(root, path, language)
}

fn print_help(options: &Options) -> i32 {
    let ending = if options.message.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    if options.exit_code == 0 {
        print!("{}{ending}", options.message);
    } else {
        eprint!("{}{ending}", options.message);
    }
    options.exit_code
}

fn restore(root: &Path) {
    for path in restore_backups(root) {
        eprintln!("Restored {} from an interrupted mutation.", path.display());
    }
}

fn write_stdout_bytes(bytes: &[u8]) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(256 * 1024, stdout.lock());
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

struct CacheSession {
    path: Option<PathBuf>,
    cache: SiteCache,
}

impl CacheSession {
    fn open(no_cache: bool, root: &Path) -> Self {
        if no_cache {
            return Self {
                path: None,
                cache: SiteCache::default(),
            };
        }
        let path = repo_cache_path(root);
        let cache = SiteCache::load_from(&path);
        Self {
            path: Some(path),
            cache,
        }
    }

    fn enabled(&self) -> bool {
        self.path.is_some()
    }

    fn save_misses(&mut self, misses: Vec<(String, CachedSites)>, active_keys: &HashSet<String>) {
        if misses.is_empty() {
            return;
        }
        if let Some(path) = self.path.as_deref() {
            self.cache.merge_and_save_to(path, misses, active_keys);
        }
    }
}

struct FileCacheLookup<'a> {
    canonical_key: String,
    file_key: Option<String>,
    language: &'static str,
    stamp: Option<FileStamp>,
    cached_sites: Option<&'a [Site]>,
}

fn lookup_file_cache_resolved<'a, P: CliProvider>(
    session: &'a CacheSession,
    manifests: &mut ManifestStampCache,
    resolved: &Path,
    resolved_root: &Path,
    provider: &P,
) -> FileCacheLookup<'a> {
    let canonical_key = to_posix(resolved);
    let language = DiscoverProvider::language_of(provider, resolved).unwrap_or("");
    let file_key = source_key_resolved(resolved, resolved_root).ok();
    if !session.enabled() {
        return FileCacheLookup {
            canonical_key,
            file_key,
            language,
            stamp: None,
            cached_sites: None,
        };
    }
    let stamp = file_stamp(resolved, language, manifests);
    let cached_sites = match (&file_key, stamp) {
        (Some(fk), Some(st)) => session.cache.get(&canonical_key, st, fk, language),
        _ => None,
    };
    FileCacheLookup {
        canonical_key,
        file_key,
        language,
        stamp,
        cached_sites,
    }
}

fn print_scan_text(scanned: &[ScanFileData]) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(256 * 1024, stdout.lock());
    for item in scanned {
        let text = format_scan(
            &item.file,
            &item.sites,
            &item.changed,
            item.covered.as_ref(),
        );
        let _ = out.write_all(text.as_bytes());
    }
    let _ = out.flush();
}

fn scan<P: CliProvider + Sync>(
    options: &Options,
    root: &Path,
    files: &[PathBuf],
    provider: &P,
) -> i32 {
    let mut session = CacheSession::open(options.no_cache, root);
    let history_index = HistoryIndex::load(root);
    let empty_history = History::default();
    let results: Vec<(FileCacheLookup<'_>, Result<ScanFileData, String>)> = files
        .par_iter()
        .map_init(ManifestStampCache::new, |manifests, path| {
            let lookup = lookup_file_cache_resolved(&session, manifests, path, root, provider);
            let cov = coverage_for(options, root, path, provider);
            let history = lookup
                .file_key
                .as_deref()
                .and_then(|key| history_index.get(key))
                .unwrap_or(&empty_history);
            let scan_opts = ScanOptions {
                covered_lines: cov.as_ref(),
                ignore_coverage: options.no_coverage,
                lines: options.lines.as_ref(),
                cached_sites: lookup.cached_sites,
            };
            let outcome = match lookup.file_key.as_deref() {
                Some(file_key) => scan_file_resolved_with_history(
                    path, root, file_key, &scan_opts, history, provider,
                ),
                None => scan_file_cached_with_history(path, root, &scan_opts, history, provider),
            };
            (lookup, outcome)
        })
        .collect();

    let mut scanned = Vec::with_capacity(results.len());
    let mut misses = Vec::new();
    let mut active_keys = HashSet::with_capacity(files.len());
    for (lookup, outcome) in results {
        active_keys.insert(lookup.canonical_key.clone());
        match outcome {
            Ok(mut data) => {
                if session.enabled()
                    && let Some(sites) = data.discovered_sites.take()
                    && let (Some(st), Some(fk)) = (lookup.stamp, lookup.file_key)
                {
                    misses.push((
                        lookup.canonical_key,
                        CachedSites {
                            stamp: st,
                            file_key: fk,
                            language: lookup.language.to_string(),
                            sites,
                        },
                    ));
                }
                scanned.push(data);
            }
            Err(err) => {
                if !options.json {
                    print_scan_text(&scanned);
                }
                eprintln!("{err}");
                return 2;
            }
        }
    }
    session.save_misses(misses, &active_keys);
    if options.json {
        write_stdout_bytes(format_scan_json(&scanned).as_bytes());
    } else {
        print_scan_text(&scanned);
    }
    0
}

#[derive(Default)]
struct MutateAccumulator {
    forms: Vec<FormResult>,
    sites: Vec<MutateSite>,
    written: Vec<String>,
    skipped: Vec<SkippedFile>,
    baseline_failed: bool,
    survived: bool,
}

fn record_result(result: RunResult, json: bool, acc: &mut MutateAccumulator) {
    if !result.skipped.is_empty() {
        eprintln!("Skipping {}: {}", result.path, result.skipped);
        acc.skipped.push(SkippedFile {
            file: result.path,
            reason: result.skipped,
        });
        return;
    }
    if result.baseline_failed {
        eprintln!("{}", result.baseline_message);
        acc.baseline_failed = true;
        return;
    }
    if json {
        for site in result.sites {
            let status = result.statuses.get(&site.mutation_id).cloned();
            acc.sites.push(MutateSite { site, status });
        }
    } else {
        print!("{}", format_site_log(&result.sites, &result.statuses));
    }
    if result.forms.iter().any(|f| f.survived > 0) {
        acc.survived = true;
    }
    acc.forms.extend(result.forms);
    acc.written.extend(result.written);
}

fn finish(baseline_failed: bool, survived: bool) -> i32 {
    if baseline_failed {
        return 2;
    }
    if survived {
        return 3;
    }
    0
}

fn mutate_files<P: CliProvider + Sync>(
    options: &Options,
    root: &Path,
    files: &[PathBuf],
    provider: &P,
) -> i32 {
    let mut session = CacheSession::open(options.no_cache, root);
    let mut manifests = ManifestStampCache::new();
    let runner = CommandRunner::new(options.verbose);
    let mut baselines = BaselineCache::new();
    let history_index = HistoryIndex::load(root);
    let empty_history = History::default();
    let mut worker_ctx = WorkerContext::new();
    let mut acc = MutateAccumulator::default();
    let mut misses = Vec::new();
    let mut active_keys = HashSet::with_capacity(files.len());

    for path in files {
        let cov = coverage_for(options, root, path, provider);
        let opts = MutateOptions {
            covered_lines: cov.as_ref(),
            ignore_coverage: options.no_coverage,
            mutate_all: options.mutate_all,
            lines: options.lines.as_ref(),
            test_command: options.test_command.as_deref(),
            timeout_factor: options.timeout_factor,
            mutation_warning: options.mutation_warning,
            max_workers: options.max_workers,
            xcode_scheme: options.xcode_scheme.as_deref(),
            xcode_destination: options.xcode_destination.as_deref(),
        };
        let (canonical_key, file_key, language, stamp, was_hit, result) = {
            let lookup = lookup_file_cache_resolved(&session, &mut manifests, path, root, provider);
            let history = lookup
                .file_key
                .as_deref()
                .and_then(|key| history_index.get(key))
                .unwrap_or(&empty_history);
            let result = mutate_file_cached_with_context(
                path,
                root,
                &runner,
                &opts,
                &mut baselines,
                history,
                lookup.cached_sites,
                &mut worker_ctx,
                provider,
            );
            (
                lookup.canonical_key,
                lookup.file_key,
                lookup.language,
                lookup.stamp,
                lookup.cached_sites.is_some(),
                result,
            )
        };
        active_keys.insert(canonical_key.clone());
        if session.enabled()
            && !was_hit
            && result.skipped.is_empty()
            && !result.baseline_failed
            && let (Some(st), Some(fk)) = (stamp, file_key)
        {
            misses.push((
                canonical_key,
                CachedSites {
                    stamp: st,
                    file_key: fk,
                    language: language.to_string(),
                    sites: result.sites.clone(),
                },
            ));
        }
        record_result(result, options.json, &mut acc);
    }

    session.save_misses(misses, &active_keys);

    if options.json {
        let report = MutateReport {
            forms: &acc.forms,
            sites: &acc.sites,
            written: &acc.written,
            skipped: &acc.skipped,
            baseline_failed: acc.baseline_failed,
        };
        write_stdout_bytes(format_mutate_json(&report).as_bytes());
    } else if !acc.forms.is_empty() {
        write_stdout_bytes(format_results(&acc.forms).as_bytes());
    }
    for path in acc.written {
        eprintln!("Wrote {path}");
    }
    finish(acc.baseline_failed, acc.survived)
}

/// Execute the full CLI pipeline with the supplied provider.
pub fn run_with<P: CliProvider + Sync>(args: &[String], provider: &P) -> i32 {
    let help = help_text(provider.skip_dirs(), provider.test_dirs());
    let options = parse_args_with_help(args, &help);
    if options.action == "help" {
        return print_help(&options);
    }
    let root = resolve_path(&options.project_root);
    let files = match select_files_with(&options, provider) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("{}", err.message);
            return err.code;
        }
    };
    if files.is_empty() {
        if options.json {
            if options.scan {
                print!("{}", format_scan_json(&[]));
            } else {
                print!("{}", format_mutate_json(&MutateReport::default()));
            }
        } else {
            println!("No source files to mutate.");
        }
        return 0;
    }
    if options.scan {
        let _slot = ub_runtime::acquire_global_scan_slot().ok().flatten();
        return scan(&options, &root, &files, provider);
    }
    let _repo_lock = ub_runtime::acquire_repo_lock(
        "mutator",
        &root,
        Some(&format!(
            "Waiting for another mutator run on {}...",
            root.display()
        )),
    )
    .ok();
    restore(&root);
    let coverage = prepare_coverage(&options, &root, &files, provider);
    if coverage != 0 {
        return coverage;
    }
    mutate_files(&options, &root, &files, provider)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn parse_args_validates_flags_and_conflicts() {
        let help = help_text(FALLBACK_SKIP_DIRS, FALLBACK_TEST_DIRS);
        assert_eq!(parse_args_with_help(&s(&["--help"]), &help).exit_code, 0);
        assert!(help.contains("uml-viewer"));
        assert!(help.contains("__pycache__"));
        assert!(help.contains("testdata"));
        assert!(help.contains("--json"));
        assert!(help.contains("--no-cache"));

        let json_no_cache = parse_args_with_help(&s(&["--scan", "--json", "--no-cache"]), &help);
        assert!(json_no_cache.json);
        assert!(json_no_cache.no_cache);
        assert!(json_no_cache.scan);

        let lines = parse_args_with_help(&s(&["--lines", "2, 4", "src/demo.py"]), &help);
        assert_eq!(lines.lines, Some(HashSet::from([2, 4])));
        assert_eq!(lines.positionals, vec!["src/demo.py"]);

        assert_eq!(
            parse_args_with_help(&s(&["--mutate-all", "--since-last-run"]), &help).exit_code,
            1
        );
        assert_eq!(
            parse_args_with_help(&s(&["--lines", "0"]), &help).exit_code,
            1
        );
        assert_eq!(
            parse_args_with_help(&s(&["--timeout-factor", "0"]), &help).exit_code,
            1
        );
        assert_eq!(
            parse_args_with_help(&s(&["--max-workers", "0"]), &help).exit_code,
            1
        );
        assert_eq!(
            parse_args_with_help(&s(&["--scan", "--max-workers", "2"]), &help).exit_code,
            1
        );
        assert_eq!(
            parse_args_with_help(&s(&["--not-a-flag"]), &help).exit_code,
            1
        );

        let big_workers =
            parse_args_with_help(&s(&["--max-workers", "18446744073709551616"]), &help);
        assert_eq!(big_workers.exit_code, 0);
        assert_eq!(big_workers.max_workers, Some(usize::MAX));

        let big_lines = parse_args_with_help(&s(&["--lines", "18446744073709551616"]), &help);
        assert_eq!(big_lines.exit_code, 0);
        assert_eq!(big_lines.lines, Some(HashSet::from([usize::MAX])));

        let zwj_err = parse_args_with_help(&s(&["--lines", "👨\u{200d}👩 family"]), &help);
        assert!(
            zwj_err
                .message
                .starts_with("invalid literal for int() with base 10: '👨\\u200d👩 family'")
        );
        let nbsp_err = parse_args_with_help(&s(&["--lines", "a\u{a0}b"]), &help);
        assert!(
            nbsp_err
                .message
                .starts_with("invalid literal for int() with base 10: 'a\\xa0b'")
        );
        let accent_err = parse_args_with_help(&s(&["--lines", "e\u{301}"]), &help);
        assert!(
            accent_err
                .message
                .starts_with("invalid literal for int() with base 10: 'e\u{301}'")
        );
    }

    #[test]
    fn comma_and_and_git_changed_files() {
        assert_eq!(comma_and(&["only"]), "only");
        assert_eq!(comma_and(&["b", "a"]), "a, and b");
        assert_eq!(comma_and(&["c", "a", "b"]), "a, b, and c");

        let outside =
            std::env::temp_dir().join(format!("mutator_outside_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        assert!(changed_files(&outside).is_err());

        let repo = std::env::temp_dir().join(format!("mutator_git_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let file = repo.join("src").join("demo.py");
        std::fs::write(&file, "def place(x):\n    return x > 0\n").unwrap();
        ProcessCommand::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .args(["add", "src/demo.py"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let found = changed_files(&repo).unwrap();
        assert_eq!(found, vec![resolve_path(&file)]);

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&repo);
    }

    struct CoverageLockTestProvider(std::sync::atomic::AtomicUsize);

    impl DiscoverProvider for CoverageLockTestProvider {
        fn language_of(&self, _path: &Path) -> Option<&'static str> {
            Some("python")
        }
        fn is_test_filename(&self, _file_name: &str) -> bool {
            false
        }
        fn skip_dirs(&self) -> &[&str] {
            FALLBACK_SKIP_DIRS
        }
        fn test_dirs(&self) -> &[&str] {
            FALLBACK_TEST_DIRS
        }
    }

    impl EngineProvider for CoverageLockTestProvider {
        fn sites_in_file(
            &self,
            _source: &str,
            _path: &Path,
            _root: &Path,
            _file_key: &str,
        ) -> Vec<crate::model::Site> {
            Vec::new()
        }
        fn form_digests(
            &self,
            _source: &str,
            _path: &Path,
            _root: &Path,
        ) -> std::collections::HashMap<(String, String), String> {
            std::collections::HashMap::new()
        }
        fn form_spans(
            &self,
            _source: &str,
            _path: &Path,
            _root: &Path,
        ) -> crate::engine::FormSpanMap {
            std::collections::HashMap::new()
        }
        fn test_plan(
            &self,
            root: &Path,
            _source: &Path,
            _language: &str,
            _override_cmd: Option<&str>,
            _xcode_scheme: Option<&str>,
            _xcode_destination: Option<&str>,
        ) -> (crate::runner::Command, PathBuf) {
            (
                crate::runner::Command::Argv(vec!["true".into()]),
                root.to_path_buf(),
            )
        }
    }

    impl CliProvider for CoverageLockTestProvider {
        fn run_coverage(&self, _root: &Path, _files: &[PathBuf]) -> i32 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            0
        }
        fn covered_lines(
            &self,
            _root: &Path,
            _path: &Path,
            _language: &str,
        ) -> Option<HashSet<usize>> {
            None
        }
    }

    #[test]
    fn prepare_coverage_locks_only_when_generating_coverage() {
        let root = std::env::temp_dir().join(format!("mutator_cov_lock_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let cov_guard = ub_runtime::acquire_repo_lock("coverage", &root, None).unwrap();
        let provider = CoverageLockTestProvider(std::sync::atomic::AtomicUsize::new(0));

        // Fast-path flags must not wait on the coverage lock or invoke run_coverage.
        for opts in [
            Options {
                no_coverage: true,
                ..Default::default()
            },
            Options {
                use_existing_coverage: true,
                ..Default::default()
            },
            Options {
                scan: true,
                ..Default::default()
            },
        ] {
            assert_eq!(prepare_coverage(&opts, &root, &[], &provider), 0);
        }
        assert_eq!(provider.0.load(std::sync::atomic::Ordering::Acquire), 0);

        // Generating coverage must block on the repo coverage lock until released.
        let (tx, rx) = std::sync::mpsc::channel();
        let root_bg = root.clone();
        let handle = std::thread::spawn(move || {
            let bg_provider = CoverageLockTestProvider(std::sync::atomic::AtomicUsize::new(0));
            let code = prepare_coverage(&Options::default(), &root_bg, &[], &bg_provider);
            tx.send((
                code,
                bg_provider.0.load(std::sync::atomic::Ordering::Acquire),
            ))
            .unwrap();
        });

        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(80))
                .is_err(),
            "prepare_coverage should block while coverage lock is held"
        );
        drop(cov_guard);
        let (code, calls) = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("prepare_coverage should complete after coverage lock drop");
        assert_eq!(code, 0);
        assert_eq!(calls, 1);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_no_cache_respected_except_on_help_in_isolated_subprocess() {
        const MARKER: &str = "MUTATOR_CLI_NO_CACHE_SUBPROC";
        if std::env::var_os(MARKER).is_some() {
            let help = help_text(FALLBACK_SKIP_DIRS, FALLBACK_TEST_DIRS);
            let parsed_run = parse_args_with_help(&s(&["--scan"]), &help);
            assert_eq!(parsed_run.action, "mutate");
            assert!(parsed_run.no_cache);

            let parsed_help = parse_args_with_help(&s(&["--help"]), &help);
            assert_eq!(parsed_help.action, "help");
            assert!(!parsed_help.no_cache);

            let parsed_err = parse_args_with_help(&s(&["--not-a-flag"]), &help);
            assert_eq!(parsed_err.action, "help");
            assert!(!parsed_err.no_cache);
            return;
        }

        let exe = std::env::current_exe().expect("current test exe");
        let output = ProcessCommand::new(exe)
            .args([
                "--exact",
                "cli::tests::env_no_cache_respected_except_on_help_in_isolated_subprocess",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .env("UB_TOOLS_NO_CACHE", "1")
            .output()
            .expect("spawn subprocess");
        assert!(
            output.status.success(),
            "subprocess failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn scan_uses_cache_avoids_rewrite_on_hit_and_respects_no_cache() {
        struct CountingSpy {
            sites_calls: std::sync::atomic::AtomicUsize,
        }
        impl DiscoverProvider for CountingSpy {
            fn language_of(&self, _path: &Path) -> Option<&'static str> {
                Some("python")
            }
            fn is_test_filename(&self, _file_name: &str) -> bool {
                false
            }
            fn skip_dirs(&self) -> &[&str] {
                FALLBACK_SKIP_DIRS
            }
            fn test_dirs(&self) -> &[&str] {
                FALLBACK_TEST_DIRS
            }
        }
        impl EngineProvider for CountingSpy {
            fn sites_in_file(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
                file_key: &str,
            ) -> Vec<Site> {
                self.sites_calls
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                vec![Site {
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
                    mutation_id: "m".to_string(),
                    anchor_line: 2,
                }]
            }
            fn form_digests(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
            ) -> HashMap<(String, String), String> {
                HashMap::new()
            }
            fn form_spans(
                &self,
                _source: &str,
                _path: &Path,
                _root: &Path,
            ) -> crate::engine::FormSpanMap {
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
            ) -> (crate::runner::Command, PathBuf) {
                (
                    crate::runner::Command::Argv(vec!["true".into()]),
                    root.to_path_buf(),
                )
            }
        }
        impl CliProvider for CountingSpy {
            fn run_coverage(&self, _root: &Path, _files: &[PathBuf]) -> i32 {
                0
            }
            fn covered_lines(
                &self,
                _root: &Path,
                _path: &Path,
                _language: &str,
            ) -> Option<HashSet<usize>> {
                None
            }
        }

        let root = std::env::temp_dir().join(format!("mutator_cli_cache_{}", uuid::Uuid::new_v4()));
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
                let _ = std::fs::remove_file(&self.1);
            }
        }
        let cache_path = repo_cache_path(&root);
        let _cleanup = Cleanup(root.clone(), cache_path.clone());

        let file = root.join("src").join("demo.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "def f(x):\n    return x > 0\n").unwrap();
        let root_str = root.to_string_lossy().into_owned();
        let spy = CountingSpy {
            sites_calls: std::sync::atomic::AtomicUsize::new(0),
        };

        // 1. Cold scan misses cache, calls sites_in_file once, writes cache once.
        let code = run_with(
            &s(&[
                "--scan",
                "--no-coverage",
                "--root",
                &root_str,
                "src/demo.py",
            ]),
            &spy,
        );
        assert_eq!(code, 0);
        assert_eq!(
            spy.sites_calls.load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert!(cache_path.is_file());
        let first_bytes = std::fs::read(&cache_path).unwrap();

        // 2. Warm scan hits cache: sites_in_file count unchanged, cache file untouched.
        let sentinel_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .unwrap()
            .set_modified(sentinel_time)
            .unwrap();
        let stamp_before = ub_runtime::file_mtime_size(&cache_path).unwrap();
        assert_eq!(
            run_with(
                &s(&[
                    "--scan",
                    "--no-coverage",
                    "--root",
                    &root_str,
                    "src/demo.py"
                ]),
                &spy,
            ),
            0
        );
        assert_eq!(
            spy.sites_calls.load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert_eq!(
            ub_runtime::file_mtime_size(&cache_path).unwrap(),
            stamp_before
        );
        assert_eq!(std::fs::read(&cache_path).unwrap().len(), first_bytes.len());

        // 3. --no-cache bypasses existing cache and forces re-discovery without rewriting cache.
        assert_eq!(
            run_with(
                &s(&[
                    "--scan",
                    "--no-coverage",
                    "--no-cache",
                    "--root",
                    &root_str,
                    "src/demo.py"
                ]),
                &spy,
            ),
            0
        );
        assert_eq!(
            spy.sites_calls.load(std::sync::atomic::Ordering::Acquire),
            2
        );
        assert_eq!(
            ub_runtime::file_mtime_size(&cache_path).unwrap(),
            stamp_before
        );
    }
}
