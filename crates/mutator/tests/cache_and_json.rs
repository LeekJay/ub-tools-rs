use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_mutator(cwd: &Path, cache_dir: &Path, args: &[&str], extra_envs: &[(&str, &str)]) -> Output {
    let bin = Path::new(env!("CARGO_BIN_EXE_mutator"));
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("UB_TOOLS_CACHE_DIR", cache_dir)
        .env_remove("UB_TOOLS_NO_CACHE")
        .env_remove("UB_TOOLS_NO_METRICS");
    for (k, v) in extra_envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run mutator binary")
}

fn single_cache_file(cache_dir: &Path) -> PathBuf {
    let files: Vec<PathBuf> = fs::read_dir(cache_dir)
        .expect("read cache dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("cache"))
        .collect();
    assert_eq!(files.len(), 1, "expected 1 cache file, got {files:?}");
    files.into_iter().next().unwrap()
}

fn python_bin() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[test]
fn warm_cache_matches_uncached_scan_skips_read_and_invalidates_on_edits_and_corrupt_cache() {
    let repo = TempDirGuard::new("mutator_it_scan_cache");
    let cache_dir = TempDirGuard::new("mutator_it_cache_dir");
    let root = &repo.0;

    fs::create_dir_all(root.join("src")).unwrap();
    let py_file = root.join("src").join("demo.py");
    let py_src = "def add(a, b):\n    if a > 0:\n        return a + b\n    return 0\n";
    fs::write(&py_file, py_src).unwrap();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let rs_file = root.join("src").join("lib.rs");
    fs::write(&rs_file, "pub fn inc(x: i32) -> i32 { x + 1 }\n").unwrap();

    // Uncached baseline output.
    let uncached = run_mutator(
        root,
        &cache_dir.0,
        &["--scan", "--no-coverage", "--no-cache"],
        &[],
    );
    assert_eq!(uncached.status.code(), Some(0));
    assert!(
        !cache_dir.0.exists() || fs::read_dir(&cache_dir.0).unwrap().next().is_none(),
        "--no-cache must not create cache files"
    );
    let uncached_stdout = String::from_utf8(uncached.stdout).unwrap();
    assert!(uncached_stdout.contains("src/demo.py"));
    assert!(uncached_stdout.contains("src/lib.rs"));

    // UB_TOOLS_NO_CACHE=1 also bypasses cache creation.
    let env_disabled = run_mutator(
        root,
        &cache_dir.0,
        &["--scan", "--no-coverage"],
        &[("UB_TOOLS_NO_CACHE", "1")],
    );
    assert_eq!(env_disabled.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(env_disabled.stdout).unwrap(),
        uncached_stdout
    );
    assert!(
        !cache_dir.0.exists() || fs::read_dir(&cache_dir.0).unwrap().next().is_none(),
        "UB_TOOLS_NO_CACHE=1 must not create cache files"
    );

    // Cold cached run populates cache and matches uncached output verbatim.
    let cold = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(cold.status.code(), Some(0));
    assert_eq!(String::from_utf8(cold.stdout).unwrap(), uncached_stdout);
    let cache_file = single_cache_file(&cache_dir.0);
    let cache_bytes = fs::read(&cache_file).unwrap();

    // Warm run with same mtime+size: replace demo.py with invalid UTF-8 of identical byte length and
    // restore original mtime to prove fs::read is completely skipped when history is empty.
    let orig_mtime = fs::metadata(&py_file).unwrap().modified().unwrap();
    let mut invalid_bytes = vec![0xFF; py_src.len()];
    invalid_bytes[0] = b'#';
    fs::write(&py_file, &invalid_bytes).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&py_file)
        .unwrap()
        .set_modified(orig_mtime)
        .unwrap();
    assert_eq!(fs::metadata(&py_file).unwrap().len(), py_src.len() as u64);

    let warm = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warm scan unexpectedly read invalid UTF-8 bytes; stderr: {}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert_eq!(String::from_utf8(warm.stdout).unwrap(), uncached_stdout);
    assert_eq!(fs::read(&cache_file).unwrap(), cache_bytes);

    // Restore valid Python and verify manifest edit (`Cargo.toml`) invalidates cached Rust sites.
    fs::write(&py_file, py_src).unwrap();
    let warm_restored = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(warm_restored.status.code(), Some(0));
    let before_manifest_edit = fs::read(&cache_file).unwrap();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let after_manifest = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(after_manifest.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(after_manifest.stdout).unwrap(),
        uncached_stdout
    );
    let after_manifest_bytes = fs::read(&cache_file).unwrap();
    assert_ne!(after_manifest_bytes, before_manifest_edit);

    // Source edit invalidates and reflects new sites.
    fs::write(
        &py_file,
        "def add(a, b):\n    if a >= 0:\n        return a - b\n    return 0\n",
    )
    .unwrap();
    let after_source_edit = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(after_source_edit.status.code(), Some(0));
    let edited_stdout = String::from_utf8(after_source_edit.stdout).unwrap();
    assert_ne!(edited_stdout, uncached_stdout);
    assert!(edited_stdout.contains(">="));

    // Corrupt cache is tolerated cleanly and healed on next run.
    fs::write(&cache_file, b"CORRUPT_UBC_BLOB").unwrap();
    let healed = run_mutator(root, &cache_dir.0, &["--scan", "--no-coverage"], &[]);
    assert_eq!(healed.status.code(), Some(0));
    assert_eq!(String::from_utf8(healed.stdout).unwrap(), edited_stdout);
    assert!(fs::read(&cache_file).unwrap().starts_with(b"UBC1"));
}

#[test]
fn json_outputs_empty_scan_and_mutate_exit_codes_cleanly() {
    let empty_repo = TempDirGuard::new("mutator_it_empty_json");
    let cache_dir = TempDirGuard::new("mutator_it_json_cache");

    // Empty repo --scan --json.
    let empty_scan = run_mutator(
        &empty_repo.0,
        &cache_dir.0,
        &["--scan", "--no-coverage", "--json"],
        &[],
    );
    assert_eq!(empty_scan.status.code(), Some(0));
    let es_val: serde_json::Value =
        serde_json::from_slice(&empty_scan.stdout).expect("valid empty scan json");
    assert_eq!(es_val["version"], 1);
    assert_eq!(es_val["tool"], "mutator");
    assert_eq!(es_val["mode"], "scan");
    assert_eq!(es_val["summary"]["files"], 0);
    assert_eq!(es_val["summary"]["sites"], 0);
    assert_eq!(es_val["files"], serde_json::json!([]));

    // Empty repo mutate --json.
    let empty_mutate = run_mutator(
        &empty_repo.0,
        &cache_dir.0,
        &["--no-coverage", "--json"],
        &[],
    );
    assert_eq!(empty_mutate.status.code(), Some(0));
    let em_val: serde_json::Value =
        serde_json::from_slice(&empty_mutate.stdout).expect("valid empty mutate json");
    assert_eq!(em_val["version"], 1);
    assert_eq!(em_val["tool"], "mutator");
    assert_eq!(em_val["mode"], "mutate");
    assert_eq!(em_val["summary"]["sites"], 0);
    assert_eq!(em_val["summary"]["score"], serde_json::Value::Null);
    assert_eq!(em_val["summary"]["baseline_failed"], false);

    // Populated Python repo for --scan --json and mutation exit codes 0, 3, 2.
    let repo = TempDirGuard::new("mutator_it_mutate_json");
    let root = &repo.0;
    fs::create_dir_all(root.join("src")).unwrap();
    let py_file = root.join("src").join("demo.py");
    fs::write(
        &py_file,
        "def add(a, b):\n    if a > 0:\n        return a + b\n    return 0\n",
    )
    .unwrap();

    let scan_json = run_mutator(
        root,
        &cache_dir.0,
        &["--scan", "--no-coverage", "--json"],
        &[],
    );
    assert_eq!(scan_json.status.code(), Some(0));
    assert!(scan_json.stderr.is_empty());
    let scan_str = String::from_utf8(scan_json.stdout).unwrap();
    assert!(!scan_str.contains("Scan:"));
    let scan_val: serde_json::Value = serde_json::from_str(&scan_str).unwrap();
    assert_eq!(scan_val["mode"], "scan");
    assert_eq!(scan_val["summary"]["files"], 1);
    assert!(scan_val["summary"]["sites"].as_u64().unwrap() >= 2);
    assert_eq!(scan_val["summary"]["covered"], 0);
    assert_eq!(scan_val["summary"]["uncovered"], 0);
    assert_eq!(
        scan_val["files"][0]["sites"][0]["covered"],
        serde_json::Value::Null
    );

    let py = python_bin();
    let pass_cmd = format!(
        "{py} -c \"import sys; sys.path.insert(0, 'src'); from demo import add; raise SystemExit(0 if add(2, 3) == 5 else 1)\""
    );

    // Exit 0: line 3 only (`a + b` killed).
    let killed = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--lines",
            "3",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(killed.status.code(), Some(0));
    let killed_stdout = String::from_utf8(killed.stdout).unwrap();
    let killed_stderr = String::from_utf8_lossy(&killed.stderr);
    assert!(!killed_stdout.contains("KILLED"));
    assert!(!killed_stdout.contains("Wrote"));
    assert!(killed_stderr.contains("[mutator] START src/demo.py (1 site, 1 worker, baseline "));
    assert!(killed_stderr.contains("[mutator] [1/1] KILLED   src/demo.py:3 "));
    assert!(
        killed_stderr
            .contains("[mutator] DONE  src/demo.py (killed=1, survived=0, timeout=0, elapsed=")
    );
    assert!(killed_stderr.contains("Wrote "));
    assert!(killed_stderr.contains("[mutator] PASS (exit 0): all 1 tested mutant killed"));
    let killed_val: serde_json::Value = serde_json::from_str(&killed_stdout).unwrap();
    assert_eq!(killed_val["mode"], "mutate");
    assert_eq!(killed_val["summary"]["baseline_failed"], false);
    assert!(killed_val["summary"]["killed"].as_u64().unwrap() >= 1);
    assert_eq!(killed_val["summary"]["survived"], 0);
    assert_eq!(killed_val["summary"]["score"], 1.0);
    assert!(!killed_val["written"].as_array().unwrap().is_empty());

    // Exit 3: full file (at least one mutant survives).
    let survived = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(survived.status.code(), Some(3));
    let survived_stdout = String::from_utf8(survived.stdout).unwrap();
    let survived_stderr = String::from_utf8_lossy(&survived.stderr);
    assert!(!survived_stdout.contains("SURVIVED"));
    let survived_val: serde_json::Value = serde_json::from_str(&survived_stdout).unwrap();
    let total = survived_val["summary"]["sites"].as_u64().unwrap();
    assert!(survived_stderr.contains(&format!(
        "[mutator] START src/demo.py ({total} sites, 1 worker, baseline "
    )));
    assert!(survived_stderr.contains(&format!("[mutator] [{total}/{total}] ")));
    assert!(survived_stderr.contains("SURVIVED src/demo.py:"));
    assert!(survived_stderr.contains("[mutator] DONE  src/demo.py ("));
    assert!(survived_stderr.contains("[mutator] FAIL (exit 3): "));
    assert!(survived_val["summary"]["survived"].as_u64().unwrap() >= 1);
    assert!(survived_val["summary"]["score"].as_f64().unwrap() < 1.0);
    assert!(
        survived_val["sites"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["status"] == "survived")
    );

    // Exit 2: baseline failure.
    let fail_cmd = format!("{py} -c \"raise SystemExit(1)\"");
    let baseline = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--json",
            "--test-command",
            &fail_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(baseline.status.code(), Some(2));
    let baseline_stderr = String::from_utf8_lossy(&baseline.stderr);
    assert!(baseline_stderr.contains("Baseline failed"));
    assert!(
        baseline_stderr.contains("[mutator] FAIL (exit 2): baseline tests failed before mutation.")
    );
    let base_val: serde_json::Value = serde_json::from_slice(&baseline.stdout).unwrap();
    assert_eq!(base_val["mode"], "mutate");
    assert_eq!(base_val["summary"]["baseline_failed"], true);
    assert_eq!(base_val["summary"]["score"], serde_json::Value::Null);
}

#[test]
fn no_metrics_and_this_run_vs_carried_forward_work_as_designed() {
    let repo = TempDirGuard::new("mutator_it_no_metrics_split");
    let cache_dir = TempDirGuard::new("mutator_it_nm_cache");
    let root = &repo.0;
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/demo.py"),
        "def add(a, b):\n    if a > 0:\n        return a + b\n    return 0\n",
    )
    .unwrap();

    let py = python_bin();
    let pass_cmd = format!(
        "{py} -c \"import sys; sys.path.insert(0, 'src'); from demo import add; raise SystemExit(0 if add(2, 3) == 5 else 1)\""
    );

    // 1. `--no-metrics` executes mutants without creating `.metrics/mutate` or emitting `Wrote`.
    let no_metrics = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--no-metrics",
            "--lines",
            "3",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(no_metrics.status.code(), Some(0));
    assert!(!root.join(".metrics").exists());
    let nm_stderr = String::from_utf8_lossy(&no_metrics.stderr);
    assert!(!nm_stderr.contains("Wrote "));
    let nm_val: serde_json::Value = serde_json::from_slice(&no_metrics.stdout).unwrap();
    assert_eq!(nm_val["written"], serde_json::json!([]));
    assert!(nm_val["summary"]["executed_killed"].as_u64().unwrap() >= 1);
    assert_eq!(nm_val["summary"]["executed_survived"], 0);
    let nm_sites = nm_val["sites"].as_array().unwrap();
    assert!(!nm_sites.is_empty());
    assert_eq!(
        nm_val["summary"]["sites"].as_u64().unwrap() as usize,
        nm_sites.len()
    );
    assert!(
        nm_sites
            .iter()
            .all(|s| s["line"] == 3 && !s["status"].is_null()),
        "line-scoped JSON must not emit status: null or off-line sites: {nm_sites:?}"
    );

    // 2. Full file run records survivor(s) on line 2 into `.metrics/mutate/*.edn`.
    let full = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(full.status.code(), Some(3));
    assert!(root.join(".metrics").join("mutate").exists());

    // 3. Targeted re-run on line 3 kills all executed sites (`executed_survived == 0`), while carried-forward
    // survivor(s) cause exit 3 and are clearly separated in stderr and JSON summary.
    let targeted = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--mutate-all",
            "--lines",
            "3",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/demo.py",
        ],
        &[],
    );
    assert_eq!(targeted.status.code(), Some(3));
    let tgt_stderr = String::from_utf8_lossy(&targeted.stderr);
    assert!(
        tgt_stderr.contains("this run killed"),
        "stderr: {tgt_stderr}"
    );
    assert!(
        tgt_stderr.contains("carried-forward"),
        "stderr: {tgt_stderr}"
    );
    let tgt_val: serde_json::Value = serde_json::from_slice(&targeted.stdout).unwrap();
    assert!(tgt_val["summary"]["executed_killed"].as_u64().unwrap() >= 1);
    assert_eq!(tgt_val["summary"]["executed_survived"], 0);
    assert!(tgt_val["summary"]["survived"].as_u64().unwrap() >= 1);
}

#[test]
fn changed_lines_and_base_scope_scan_and_mutate_to_git_hunks() {
    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let repo = TempDirGuard::new("mutator_it_changed_lines");
    let cache_dir = TempDirGuard::new("mutator_it_cl_cache");
    let root = &repo.0;
    git(root, &["init"]);
    git(root, &["config", "user.name", "Mutator Test"]);
    git(root, &["config", "user.email", "mutator@example.com"]);

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/demo.py"),
        "def calc(a, b):\n    if a > 0:\n        return a + b\n    return 0\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    // Edit only line 3 (`return a + b` -> `return a + b + 1`).
    fs::write(
        root.join("src/demo.py"),
        "def calc(a, b):\n    if a > 0:\n        return a + b + 1\n    return 0\n",
    )
    .unwrap();

    let scan_changed = run_mutator(
        root,
        &cache_dir.0,
        &["--scan", "--no-coverage", "--changed-lines", "--json"],
        &[],
    );
    assert_eq!(scan_changed.status.code(), Some(0));
    assert!(scan_changed.stderr.is_empty());
    let scan_val: serde_json::Value = serde_json::from_slice(&scan_changed.stdout).unwrap();
    let sites = scan_val["files"][0]["sites"].as_array().unwrap();
    assert!(!sites.is_empty());
    assert!(sites.iter().all(|s| s["line"] == 3), "sites: {sites:?}");

    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "edit line 3"]);

    let scan_base = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--scan",
            "--no-coverage",
            "--changed-lines",
            "--base",
            "HEAD~1",
            "--json",
        ],
        &[],
    );
    assert_eq!(scan_base.status.code(), Some(0));
    assert!(scan_base.stderr.is_empty());
    let base_val: serde_json::Value = serde_json::from_slice(&scan_base.stdout).unwrap();
    let base_sites = base_val["files"][0]["sites"].as_array().unwrap();
    assert!(!base_sites.is_empty());
    assert!(
        base_sites.iter().all(|s| s["line"] == 3),
        "base_sites: {base_sites:?}"
    );
}

#[test]
fn changed_lines_scan_drops_zero_site_files_and_marks_missing_lcov_uncovered() {
    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let repo = TempDirGuard::new("mutator_it_scan_compact");
    let cache_dir = TempDirGuard::new("mutator_it_scan_compact_cache");
    let root = &repo.0;
    git(root, &["init"]);
    git(root, &["config", "user.name", "Mutator Test"]);
    git(root, &["config", "user.email", "mutator@example.com"]);

    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("coverage")).unwrap();
    fs::write(
        root.join("src/active.py"),
        "def first(x):\n    if x > 0:\n        return x + 1\n    return 0\n\ndef second(y):\n    return y * 2\n",
    )
    .unwrap();
    fs::write(
        root.join("src/comment_only.py"),
        "# old comment\ndef keep(z):\n    return z + 1\n",
    )
    .unwrap();
    fs::write(
        root.join("coverage/lcov.info"),
        "SF:src/unrelated.py\nDA:1,1\nend_of_record\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    // Modify line 3 of `src/active.py` (mutable) and line 1 comment of `src/comment_only.py` (0 mutable sites).
    fs::write(
        root.join("src/active.py"),
        "def first(x):\n    if x > 0:\n        return x + 2\n    return 0\n\ndef second(y):\n    return y * 2\n",
    )
    .unwrap();
    fs::write(
        root.join("src/comment_only.py"),
        "# new comment\ndef keep(z):\n    return z + 1\n",
    )
    .unwrap();

    let scan_json = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--scan",
            "--changed-lines",
            "--use-existing-coverage",
            "--json",
        ],
        &[],
    );
    assert_eq!(scan_json.status.code(), Some(0));
    assert!(scan_json.stderr.is_empty());
    let scan_val: serde_json::Value = serde_json::from_slice(&scan_json.stdout).unwrap();
    assert_eq!(scan_val["summary"]["files"], 1);
    let site_count = scan_val["summary"]["sites"].as_u64().unwrap();
    assert!(site_count > 0);
    assert_eq!(scan_val["summary"]["covered"], 0);
    assert_eq!(scan_val["summary"]["uncovered"], site_count);
    assert_eq!(scan_val["files"][0]["file"], "src/active.py");
    assert!(
        scan_val["files"][0]["sites"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["covered"] == false)
    );

    let scan_text = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--scan",
            "--changed-lines",
            "--use-existing-coverage",
            "src/active.py",
        ],
        &[],
    );
    assert_eq!(scan_text.status.code(), Some(0));
    let text_out = String::from_utf8(scan_text.stdout).unwrap();
    let site_lines: Vec<&str> = text_out
        .lines()
        .filter(|l| l.contains("src/active.py:"))
        .collect();
    assert_eq!(site_lines.len(), site_count as usize);
    assert!(
        site_lines.iter().all(|l| l.contains(" uncovered")),
        "expected every site line in text scan to show uncovered:\n{text_out}"
    );

    // Scoped mutate on `--lines 3` emits only touched `first` form and executed line-3 site(s).
    let py = python_bin();
    let pass_cmd = format!(
        "{py} -c \"import sys; sys.path.insert(0, 'src'); from active import first; raise SystemExit(0 if first(2) == 4 else 1)\""
    );
    let scoped_run = run_mutator(
        root,
        &cache_dir.0,
        &[
            "--no-coverage",
            "--no-metrics",
            "--lines",
            "3",
            "--max-workers",
            "1",
            "--json",
            "--test-command",
            &pass_cmd,
            "src/active.py",
        ],
        &[],
    );
    assert_eq!(scoped_run.status.code(), Some(0));
    let run_val: serde_json::Value = serde_json::from_slice(&scoped_run.stdout).unwrap();
    let forms = run_val["forms"].as_array().unwrap();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["name"], "first");
    let sites = run_val["sites"].as_array().unwrap();
    assert!(!sites.is_empty());
    assert_eq!(
        run_val["summary"]["sites"].as_u64().unwrap() as usize,
        sites.len()
    );
    assert!(
        sites
            .iter()
            .all(|s| s["line"] == 3 && s["status"] == "killed"),
        "expected compact line-3 killed sites only, got: {sites:?}"
    );
}
