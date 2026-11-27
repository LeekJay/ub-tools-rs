use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::analyze::analyze_files;
use crate::coverage::{CoverageBundle, load_bundle};
use crate::discover::{
    CLOJURE_TEST_SUFFIXES, GO_TEST_SUFFIX, JS_TEST_SUFFIXES, SKIP_DIRS, TEST_DIRS,
    VUE_TEST_SUFFIXES, is_test_file, iter_source_files, language_of,
};
use crate::metrics::write_metrics;
use crate::model::Entry;
use crate::path_util::{resolve_path, to_posix};
use crate::report::format_report;
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
  --no-coverage                 Score complexity only. Coverage and CRAP are N/A.
  --use-existing-coverage       Read coverage already on disk. Do not rerun tests.
  --coverage-command <cmd>      Run this command instead of the per-language
                                coverage tools, then read the reports it wrote.
  --no-metrics                  Skip writing .metrics/crap.edn. Print report only.
  --threshold <number>          Exit 2 when the worst CRAP score is above this.

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
        }
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
                Err(msg) => {
                    return Options {
                        action: "help".into(),
                        message: format!("{msg}\n\n{help}"),
                        exit_code: 1,
                        ..Options::default()
                    };
                }
            },
            "--root" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.project_root = PathBuf::from(val);
                    index += 2;
                }
                Err(msg) => {
                    return Options {
                        action: "help".into(),
                        message: format!("{msg}\n\n{help}"),
                        exit_code: 1,
                        ..Options::default()
                    };
                }
            },
            "--coverage-command" => match take_value(args, index, arg) {
                Ok(val) => {
                    options.coverage_command = Some(val.to_string());
                    index += 2;
                }
                Err(msg) => {
                    return Options {
                        action: "help".into(),
                        message: format!("{msg}\n\n{help}"),
                        exit_code: 1,
                        ..Options::default()
                    };
                }
            },
            "--threshold" => match take_value(args, index, arg) {
                Ok(val) => match val.parse::<f64>() {
                    Ok(num) if num.is_finite() => {
                        options.threshold = Some(num);
                        index += 2;
                    }
                    _ => {
                        return Options {
                            action: "help".into(),
                            message: format!("--threshold requires a number\n\n{help}"),
                            exit_code: 1,
                            ..Options::default()
                        };
                    }
                },
                Err(msg) => {
                    return Options {
                        action: "help".into(),
                        message: format!("{msg}\n\n{help}"),
                        exit_code: 1,
                        ..Options::default()
                    };
                }
            },
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
            s if s.starts_with('-') => {
                return Options {
                    action: "help".into(),
                    message: format!("Unknown option: {s}\n\n{help}"),
                    exit_code: 1,
                    ..Options::default()
                };
            }
            _ => {
                options.positionals.push(arg.clone());
                index += 1;
            }
        }
    }

    if options.no_coverage
        && options
            .coverage_command
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    {
        return Options {
            action: "help".into(),
            message: format!("--no-coverage cannot be combined with --coverage-command\n\n{help}"),
            exit_code: 1,
            ..Options::default()
        };
    }

    options.no_metrics = options.no_metrics || ub_runtime::env_no_metrics();
    options
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub code: i32,
    pub message: String,
}

fn run_git(root: &Path, args: &[&str]) -> (i32, Vec<u8>, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(1),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(err) => (127, Vec::new(), err.to_string()),
    }
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

pub fn select_files(options: &Options) -> Result<Vec<PathBuf>, GitError> {
    let root = resolve_path(&options.project_root);
    let (existing, filters) = positionals(&root, &options.positionals);
    let picked = apply_filters(selected(options, &root, &existing)?, &filters);
    let mut unique: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for p in picked {
        let resolved = resolve_path(&p);
        unique.insert((to_posix(&resolved), resolved));
    }
    Ok(unique.into_iter().map(|(_, p)| p).collect())
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

fn threshold_status(options: &Options, entries: &[Entry], err: &mut dyn Write) -> i32 {
    let Some(threshold) = options.threshold else {
        return 0;
    };
    let worst = entries.iter().filter_map(|e| e.crap).max_by(f64::total_cmp);
    if let Some(max_crap) = worst
        && max_crap > threshold
    {
        let _ = writeln!(
            err,
            "CRAP threshold exceeded: {max_crap:.1} > {threshold:.1}"
        );
        return 2;
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
    let files = match select_files(&options) {
        Ok(f) => f,
        Err(exc) => {
            let _ = writeln!(err, "{}", exc.message);
            return exc.code;
        }
    };
    if files.is_empty() {
        let _ = writeln!(out, "No source files to analyze.");
        return 0;
    }

    let _slot = ub_runtime::acquire_global_scan_slot().ok().flatten();

    let mut bundle = coverage_bundle(&options, &root, &files);
    let entries = analyze_files(&files, &root, bundle.as_mut());
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
    let _ = write!(out, "{}", format_report(&entries));
    let _ = out.flush();
    if let Some(metrics) = written_path {
        let _ = writeln!(err, "Wrote {}", metrics.display());
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
}
