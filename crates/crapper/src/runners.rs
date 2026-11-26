use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::discover::{is_test_file, language_of};
use crate::path_util::{resolve_path, to_posix};

const MAVEN_ARGS: &[&str] = &[
    "-q",
    "org.jacoco:jacoco-maven-plugin:0.8.12:prepare-agent",
    "test",
    "org.jacoco:jacoco-maven-plugin:0.8.12:report",
];

const VITEST_CONFIGS: &[&str] = &[
    "vitest.config.ts",
    "vitest.config.mts",
    "vitest.config.cts",
    "vitest.config.js",
    "vitest.config.mjs",
    "vitest.config.cjs",
];

const PYTHON_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "pytest.ini",
    "tox.ini",
];

const COVERAGE_DIRS: &[&str] = &["typescript", "rust", "go", "python"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Shell(String),
    Args(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageJsonError(pub String);

impl std::fmt::Display for PackageJsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PackageJsonError {}

fn warn(message: &str) {
    eprintln!("{message}");
}

fn quote_posix(part: &str) -> String {
    if !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        part.to_string()
    } else {
        format!("'{}'", part.replace('\'', "'\"'\"'"))
    }
}

fn show_command(command: &ShellCommand) -> String {
    match command {
        ShellCommand::Shell(s) => s.clone(),
        ShellCommand::Args(args) => args
            .iter()
            .map(|p| quote_posix(p))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn build_command(program: &str, args: &[String], cwd: &Path) -> Command {
    #[cfg(windows)]
    {
        // On Windows, `.cmd`/`.bat` shims (npm, npx, mvn, clj) cannot be spawned directly
        // via CreateProcess without resolving or invoking through cmd.exe.
        if let Some(resolved) = which_program(program) {
            let ext = resolved
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase());
            if matches!(ext.as_deref(), Some("cmd") | Some("bat")) {
                let mut cmd = Command::new("cmd");
                cmd.arg("/C").arg(&resolved).args(args).current_dir(cwd);
                return cmd;
            }
            let mut cmd = Command::new(resolved);
            cmd.args(args).current_dir(cwd);
            return cmd;
        }
    }
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    cmd
}

pub fn which_program(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return if candidate.is_file() {
            Some(candidate.to_path_buf())
        } else {
            None
        };
    }
    let path_env = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();

    for dir in std::env::split_paths(&path_env) {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        {
            for ext in &exts {
                let with_ext = dir.join(format!("{name}{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

pub fn run_shell(command: &ShellCommand, cwd: &Path) -> i32 {
    warn(&format!("+ ({}) {}", cwd.display(), show_command(command)));
    let status = match command {
        ShellCommand::Shell(text) => {
            #[cfg(windows)]
            {
                let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
                Command::new(shell)
                    .args(["/C", text])
                    .current_dir(cwd)
                    .status()
            }
            #[cfg(not(windows))]
            {
                Command::new("/bin/sh")
                    .args(["-c", text])
                    .current_dir(cwd)
                    .status()
            }
        }
        ShellCommand::Args(args) => {
            let Some((program, rest)) = args.split_first() else {
                return 127;
            };
            build_command(program, rest, cwd).status()
        }
    };
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(exc) => {
            warn(&format!("Coverage command failed to start: {exc}"));
            127
        }
    }
}

pub fn nearest_marker(start: &Path, marker: &str, stop: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    let stop = resolve_path(stop);
    loop {
        if current.join(marker).is_file() {
            return Some(current);
        }
        if resolve_path(&current) == stop {
            return None;
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

fn clean_dir(path: &Path) {
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
}

fn remove_file(path: &Path) {
    if path.is_file() {
        let _ = fs::remove_file(path);
    }
}

fn outside_language_dirs(coverage: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(coverage) else {
        return true;
    };
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| COVERAGE_DIRS.contains(&n))
    {
        return false;
    }
    !relative.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| COVERAGE_DIRS.contains(&s))
    })
}

fn clean_clojure(root: &Path) {
    let coverage = root.join("target").join("coverage");
    if !coverage.is_dir() {
        return;
    }
    let lcov = coverage.join("lcov.info");
    remove_file(&lcov);
    let mut dirs = Vec::new();
    for entry in walkdir::WalkDir::new(&coverage).into_iter().flatten() {
        let p = entry.path();
        if entry.file_type().is_file()
            && p.extension().and_then(|e| e.to_str()) == Some("html")
            && outside_language_dirs(&coverage, p)
        {
            let _ = fs::remove_file(p);
        } else if entry.file_type().is_dir() && p != coverage {
            dirs.push(p.to_path_buf());
        }
    }
    dirs.sort();
    dirs.reverse();
    for dir in dirs {
        if outside_language_dirs(&coverage, &dir) {
            let _ = fs::remove_dir(&dir);
        }
    }
}

fn package_json(
    package: &Path,
) -> Result<Option<serde_json::Map<String, Value>>, PackageJsonError> {
    let path = package.join("package.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| PackageJsonError(format!("{}: {e}", path.display())))?;
    let val: Value = serde_json::from_str(&text)
        .map_err(|e| PackageJsonError(format!("{}: {e}", path.display())))?;
    let obj = val
        .as_object()
        .cloned()
        .ok_or_else(|| PackageJsonError(format!("{}: expected a JSON object", path.display())))?;
    Ok(Some(obj))
}

fn uses_vitest(package: &Path, scripts: &serde_json::Map<String, Value>) -> bool {
    if scripts
        .get("test")
        .and_then(Value::as_str)
        .is_some_and(|s| s.contains("vitest"))
    {
        return true;
    }
    VITEST_CONFIGS
        .iter()
        .any(|name| package.join(name).is_file())
}

fn vitest_version(package: &Path) -> Option<String> {
    let installed = package
        .join("node_modules")
        .join("vitest")
        .join("package.json");
    if !installed.is_file() {
        return None;
    }
    let text = fs::read_to_string(&installed).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(obj)) => obj
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        Ok(_) => None,
        Err(exc) => {
            warn(&format!("{}: {exc}", installed.display()));
            None
        }
    }
}

fn manifest_snapshot(package: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    ["package.json", "package-lock.json", "npm-shrinkwrap.json"]
        .into_iter()
        .map(|name| {
            let p = package.join(name);
            let bytes = if p.is_file() { fs::read(&p).ok() } else { None };
            (p, bytes)
        })
        .collect()
}

fn restore_manifests(snapshot: Vec<(PathBuf, Option<Vec<u8>>)>) {
    for (path, content) in snapshot {
        match content {
            None => remove_file(&path),
            Some(bytes) => {
                if !path.is_file() || fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                    let _ = fs::write(&path, bytes);
                }
            }
        }
    }
}

fn ensure_vitest_coverage(package: &Path) -> bool {
    let provider = package
        .join("node_modules")
        .join("@vitest")
        .join("coverage-v8");
    if provider.is_dir() {
        return true;
    }
    let spec = match vitest_version(package) {
        Some(v) => format!("@vitest/coverage-v8@{v}"),
        None => "@vitest/coverage-v8".to_string(),
    };
    warn(&format!(
        "Installing {spec} so Vitest can write LCOV. package.json and the lockfile are restored afterward."
    ));
    let snapshot = manifest_snapshot(package);
    let code = run_shell(
        &ShellCommand::Args(vec![
            "npm".into(),
            "install".into(),
            "--no-save".into(),
            "--no-package-lock".into(),
            spec,
        ]),
        package,
    );
    restore_manifests(snapshot);
    let ready = code == 0 && provider.is_dir();
    if !ready {
        warn("Vitest's coverage provider is missing. TypeScript coverage will score 0%.");
    }
    ready
}

fn is_js_language(lang: Option<&str>) -> bool {
    matches!(lang, Some("typescript") | Some("vue"))
}

pub fn typescript_packages(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolve_path(root);
    let mut packages = BTreeSet::new();
    for file in files {
        if !is_js_language(language_of(file)) {
            continue;
        }
        if let Some(pkg) = nearest_marker(file, "package.json", &root) {
            packages.insert(resolve_path(&pkg));
        }
    }
    packages.into_iter().collect()
}

fn vitest_includes(package: &Path, files: &[PathBuf]) -> Vec<String> {
    let package = resolve_path(package);
    let mut includes = Vec::new();
    for file in files {
        if !is_js_language(language_of(file)) || is_test_file(file) {
            continue;
        }
        let resolved = resolve_path(file);
        if let Ok(rel) = resolved.strip_prefix(&package) {
            includes.push(to_posix(rel));
        }
    }
    includes
}

fn vitest_coverage_command(package: &Path, files: &[PathBuf], report_dir: &Path) -> Vec<String> {
    let binary = package.join("node_modules").join(".bin").join("vitest");
    let mut command = if binary.is_file() {
        vec![binary.to_string_lossy().into_owned()]
    } else {
        vec!["npx".into(), "vitest".into()]
    };
    command.extend([
        "run".into(),
        "--coverage".into(),
        "--coverage.reporter=lcov".into(),
        format!("--coverage.reportsDirectory={}", report_dir.display()),
        "--coverage.reportOnFailure=true".into(),
    ]);
    for inc in vitest_includes(package, files) {
        command.push(format!("--coverage.include={inc}"));
    }
    command
}

pub fn typescript_command(
    package: &Path,
    files: &[PathBuf],
    report_dir: &Path,
) -> Result<Option<Vec<String>>, PackageJsonError> {
    let Some(data) = package_json(package)? else {
        return Ok(None);
    };
    let scripts = data
        .get("scripts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if scripts.contains_key("coverage") {
        return Ok(Some(vec!["npm".into(), "run".into(), "coverage".into()]));
    }
    if uses_vitest(package, &scripts) {
        return Ok(Some(vitest_coverage_command(package, files, report_dir)));
    }
    if !scripts.contains_key("test") {
        return Ok(None);
    }
    Ok(Some(vec![
        "npx".into(),
        "--yes".into(),
        "c8".into(),
        "--reporter=lcov".into(),
        "--reports-dir".into(),
        report_dir.to_string_lossy().into_owned(),
        "npm".into(),
        "test".into(),
    ]))
}

fn coverage_report(root: &Path, module: &Path, language: &str) -> PathBuf {
    let root = resolve_path(root);
    let module = resolve_path(module);
    if module == root {
        return root
            .join("target")
            .join("coverage")
            .join(language)
            .join("lcov.info");
    }
    let slug = module
        .strip_prefix(&root)
        .map(|rel| to_posix(rel).replace('/', "__"))
        .unwrap_or_else(|_| "module".to_string());
    root.join("target")
        .join("coverage")
        .join(language)
        .join(slug)
        .join("lcov.info")
}

fn modules_with(files: &[PathBuf], suffix: &str, marker: &str, root: &Path) -> Vec<PathBuf> {
    let mut modules = BTreeSet::new();
    for file in files {
        if crate::discover::path_suffix(file) != Some(suffix) {
            continue;
        }
        if let Some(m) = nearest_marker(file, marker, root) {
            modules.insert(resolve_path(&m));
        }
    }
    modules.into_iter().collect()
}

pub fn rust_modules(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolve_path(root);
    modules_with(files, ".rs", "Cargo.toml", &root)
}

fn rust_kind() -> Option<&'static str> {
    if which_program("cargo-llvm-cov").is_some() {
        return Some("llvm-cov");
    }
    if which_program("cargo-tarpaulin").is_some() {
        return Some("tarpaulin");
    }
    warn("Neither cargo-llvm-cov nor cargo-tarpaulin is installed. Installing cargo-llvm-cov.");
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if which_program("rustup").is_some() {
        let code = run_shell(
            &ShellCommand::Args(vec![
                "rustup".into(),
                "component".into(),
                "add".into(),
                "llvm-tools-preview".into(),
            ]),
            &home,
        );
        if code != 0 {
            warn("rustup component add llvm-tools-preview failed.");
        }
    }
    let code = run_shell(
        &ShellCommand::Args(vec![
            "cargo".into(),
            "install".into(),
            "cargo-llvm-cov".into(),
            "--locked".into(),
        ]),
        &home,
    );
    if code == 0 && which_program("cargo-llvm-cov").is_some() {
        return Some("llvm-cov");
    }
    warn("Rust coverage will score 0%.");
    None
}

pub fn python_roots(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolve_path(root);
    let mut found = BTreeSet::new();
    for file in files {
        if crate::discover::path_suffix(file) != Some(".py") {
            continue;
        }
        let mut module = None;
        for marker in PYTHON_MARKERS {
            module = nearest_marker(file, marker, &root);
            if module.is_some() {
                break;
            }
        }
        found.insert(resolve_path(module.as_deref().unwrap_or(&root)));
    }
    found.into_iter().collect()
}

fn python_executable(package: &Path) -> String {
    for rel in [
        ".venv/bin/python",
        "venv/bin/python",
        ".venv/Scripts/python.exe",
        "venv/Scripts/python.exe",
    ] {
        let candidate = package.join(rel);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "python3".to_string()
}

pub fn uses_pytest(package: &Path) -> bool {
    if package.join("pytest.ini").is_file() || package.join("conftest.py").is_file() {
        return true;
    }
    for name in ["pyproject.toml", "setup.cfg", "tox.ini"] {
        let p = package.join(name);
        if let Ok(text) = fs::read_to_string(&p)
            && text.contains("pytest")
        {
            return true;
        }
    }
    false
}

fn source_name(relative: &Path) -> String {
    let mut comps = relative.components();
    let first = comps
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    if comps.next().is_none() {
        return relative
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
    }
    first.unwrap_or_default()
}

pub fn python_sources(package: &Path, files: &[PathBuf]) -> String {
    let package = resolve_path(package);
    let mut tops = BTreeSet::new();
    for file in files {
        if crate::discover::path_suffix(file) != Some(".py") || is_test_file(file) {
            continue;
        }
        let resolved = resolve_path(file);
        if let Ok(rel) = resolved.strip_prefix(&package) {
            tops.insert(source_name(rel));
        }
    }
    tops.into_iter().collect::<Vec<_>>().join(",")
}

pub fn python_coverage_commands(
    py_exe: &str,
    kind: &str,
    data_file: &Path,
    report: &Path,
    source: &str,
) -> (Vec<String>, Vec<String>) {
    let mut run = vec![
        py_exe.to_string(),
        "-m".into(),
        "coverage".into(),
        "run".into(),
        format!("--data-file={}", data_file.display()),
    ];
    if !source.is_empty() {
        run.push(format!("--source={source}"));
    }
    if kind == "pytest" {
        run.extend(["-m".into(), "pytest".into()]);
    } else {
        run.extend([
            "-m".into(),
            "unittest".into(),
            "discover".into(),
            "-s".into(),
            ".".into(),
        ]);
    }
    let lcov = vec![
        py_exe.to_string(),
        "-m".into(),
        "coverage".into(),
        "lcov".into(),
        format!("--data-file={}", data_file.display()),
        "-o".into(),
        report.to_string_lossy().into_owned(),
    ];
    (run, lcov)
}

fn ensure_python_module(py_exe: &str, package: &Path, module: &str) -> bool {
    let probe = ShellCommand::Args(vec![
        py_exe.to_string(),
        "-c".into(),
        format!("import {module}"),
    ]);
    if run_shell(&probe, package) == 0 {
        return true;
    }
    warn(&format!(
        "Installing {module} for Python coverage. The project requirements are left unchanged."
    ));
    let install = ShellCommand::Args(vec![
        py_exe.to_string(),
        "-m".into(),
        "pip".into(),
        "install".into(),
        "--disable-pip-version-check".into(),
        module.to_string(),
    ]);
    run_shell(&install, package) == 0
}

pub fn rust_coverage_command(kind: &str, report: &Path) -> Vec<String> {
    if kind == "llvm-cov" {
        vec![
            "cargo".into(),
            "llvm-cov".into(),
            "--lcov".into(),
            "--output-path".into(),
            report.to_string_lossy().into_owned(),
        ]
    } else {
        let dir = report
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
        vec![
            "cargo".into(),
            "tarpaulin".into(),
            "--out".into(),
            "Lcov".into(),
            "--output-dir".into(),
            dir,
        ]
    }
}

pub fn swift_test_command() -> Vec<String> {
    vec![
        "swift".into(),
        "test".into(),
        "--enable-code-coverage".into(),
    ]
}

pub fn swift_export_command(tool: &str, binary: &Path, profdata: &Path) -> Vec<String> {
    vec![
        tool.to_string(),
        "export".into(),
        format!("-instr-profile={}", profdata.display()),
        binary.to_string_lossy().into_owned(),
    ]
}

pub fn xcode_export_command(result: &Path) -> Vec<String> {
    vec![
        "xcrun".into(),
        "xccov".into(),
        "view".into(),
        "--report".into(),
        "--json".into(),
        result.to_string_lossy().into_owned(),
    ]
}

pub fn swift_packages(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolve_path(root);
    modules_with(files, ".swift", "Package.swift", &root)
}

fn first_profdata(base: &Path, require_debug: bool) -> Option<PathBuf> {
    if !base.is_dir() {
        return None;
    }
    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(base).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if p.file_name().and_then(|n| n.to_str()) != Some("default.profdata") {
            continue;
        }
        if p.parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            != Some("codecov")
        {
            continue;
        }
        if require_debug && !p.components().any(|c| c.as_os_str() == "debug") {
            continue;
        }
        matches.push(p.to_path_buf());
    }
    matches.sort();
    matches.into_iter().next()
}

pub fn swift_profdata(package: &Path) -> Option<PathBuf> {
    let build_dir = package.join(".build");
    first_profdata(&build_dir, true).or_else(|| first_profdata(&build_dir, false))
}

fn part_ends_with(path: &Path, suffix: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.ends_with(suffix)))
}

fn is_test_executable(path: &Path) -> bool {
    if !path.is_file() || part_ends_with(path, ".dSYM") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with("PackageTests") || name.ends_with("Tests.xctest") {
        return true;
    }
    name.ends_with("Tests") && part_ends_with(path, ".xctest")
}

fn first_test_binary(base: &Path) -> Option<PathBuf> {
    if !base.is_dir() {
        return None;
    }
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(base)
        .into_iter()
        .flatten()
        .filter(|e| is_test_executable(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();
    found.sort();
    found.into_iter().next()
}

fn command_stdout(command: &[String], cwd: Option<&Path>) -> Option<String> {
    let (program, rest) = command.split_first()?;
    let mut cmd = build_command(program, rest, cwd.unwrap_or_else(|| Path::new(".")));
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn tool_stdout(name: &str, args: &[&str], cwd: Option<&Path>) -> Option<String> {
    let tool = which_program(name)?;
    let mut argv = vec![tool.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|s| (*s).to_string()));
    command_stdout(&argv, cwd)
}

pub fn swift_test_binary(package: &Path) -> Option<PathBuf> {
    if let Some(text) = tool_stdout("swift", &["build", "--show-bin-path"], Some(package)) {
        let stripped = text.trim();
        if !stripped.is_empty() {
            let p = Path::new(stripped);
            let base = if p.is_absolute() {
                p.to_path_buf()
            } else {
                package.join(p)
            };
            if let Some(found) = first_test_binary(&base) {
                return Some(found);
            }
        }
    }
    first_test_binary(&package.join(".build"))
}

pub fn llvm_cov_tool() -> Option<String> {
    if let Some(found) = which_program("llvm-cov") {
        return Some(found.to_string_lossy().into_owned());
    }
    if let Some(swift) = which_program("swift") {
        let sibling = resolve_path(&swift).parent()?.join("llvm-cov");
        if sibling.is_file() {
            return Some(sibling.to_string_lossy().into_owned());
        }
    }
    let text = tool_stdout("xcrun", &["--find", "llvm-cov"], None)?;
    let path = PathBuf::from(text.trim());
    if path.is_file() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn xcode_bundle(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    for ext in ["xcworkspace", "xcodeproj"] {
        let mut matches: Vec<PathBuf> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.extension().and_then(|e| e.to_str()) == Some(ext))
            .collect();
        matches.sort();
        if let Some(first) = matches.into_iter().next() {
            return Some(first);
        }
    }
    None
}

fn nearest_xcode(start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    let stop = resolve_path(stop);
    loop {
        if let Some(bundle) = xcode_bundle(&current) {
            return Some(bundle);
        }
        if resolve_path(&current) == stop {
            return None;
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

pub fn xcode_projects(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolve_path(root);
    let mut found = BTreeSet::new();
    for file in files {
        if crate::discover::path_suffix(file) != Some(".swift") {
            continue;
        }
        if let Some(bundle) = nearest_xcode(file, &root) {
            found.insert(resolve_path(&bundle));
        }
    }
    found.into_iter().collect()
}

pub fn xcode_schemes(project: &Path) -> Vec<String> {
    let schemes_dir = project.join("xcshareddata").join("xcschemes");
    if !schemes_dir.is_dir() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(&schemes_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("xcscheme"))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    names.sort();
    names
}

pub fn xcode_test_command(project: &Path, scheme: &str, result: &Path) -> Vec<String> {
    let flag = if project.extension().and_then(|e| e.to_str()) == Some("xcworkspace") {
        "-workspace"
    } else {
        "-project"
    };
    vec![
        "xcodebuild".into(),
        "test".into(),
        flag.into(),
        project.to_string_lossy().into_owned(),
        "-scheme".into(),
        scheme.to_string(),
        "-enableCodeCoverage".into(),
        "YES".into(),
        "-resultBundlePath".into(),
        result.to_string_lossy().into_owned(),
    ]
}

pub fn write_command_output(command: &[String], cwd: &Path, destination: &Path) -> i32 {
    warn(&format!(
        "+ ({}) {} > {}",
        cwd.display(),
        show_command(&ShellCommand::Args(command.to_vec())),
        destination.display()
    ));
    if let Some(parent) = destination.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Some((program, rest)) = command.split_first() else {
        return 127;
    };
    let Ok(mut out_file) = fs::File::create(destination) else {
        return 127;
    };
    let output = build_command(program, rest, cwd)
        .stdout(Stdio::piped())
        .output();
    match output {
        Ok(out) => {
            let _ = out_file.write_all(&out.stdout);
            out.status.code().unwrap_or(1)
        }
        Err(exc) => {
            warn(&format!("Coverage command failed to start: {exc}"));
            127
        }
    }
}

fn cover_clojure(root: &Path) {
    if !root.join("deps.edn").is_file() && !root.join("bb.edn").is_file() {
        warn("No deps.edn or bb.edn; skipping Clojure coverage.");
        return;
    }
    clean_clojure(root);
    let mut code = run_shell(
        &ShellCommand::Args(vec!["clj".into(), "-M:cov".into(), "--lcov".into()]),
        root,
    );
    if code != 0 {
        warn("clj -M:cov --lcov failed; retrying without --lcov.");
        code = run_shell(
            &ShellCommand::Args(vec!["clj".into(), "-M:cov".into()]),
            root,
        );
    }
    if code != 0 {
        warn(&format!(
            "Clojure coverage exited {code}. Clojure coverage will score 0%."
        ));
    }
}

fn cover_java(root: &Path, files: &[PathBuf]) {
    let modules = modules_with(files, ".java", "pom.xml", root);
    if modules.is_empty() {
        warn("No pom.xml; skipping Java coverage.");
        return;
    }
    let mvn_cmd = ShellCommand::Args(
        std::iter::once("mvn".to_string())
            .chain(MAVEN_ARGS.iter().map(|s| (*s).to_string()))
            .collect(),
    );
    for module in modules {
        clean_dir(&module.join("target").join("site").join("jacoco"));
        remove_file(&module.join("target").join("jacoco.exec"));
        let code = run_shell(&mvn_cmd, &module);
        if code != 0 {
            warn(&format!(
                "Java coverage exited {code} in {}. Java coverage will score 0%.",
                module.display()
            ));
        }
    }
}

fn cover_go(root: &Path, files: &[PathBuf]) {
    let modules = modules_with(files, ".go", "go.mod", root);
    if modules.is_empty() {
        warn("No go.mod; skipping Go coverage.");
        return;
    }
    for module in modules {
        let profile = module
            .join("target")
            .join("coverage")
            .join("go")
            .join("coverage.out");
        if let Some(parent) = profile.parent() {
            let _ = fs::create_dir_all(parent);
        }
        remove_file(&profile);
        let code = run_shell(
            &ShellCommand::Args(vec![
                "go".into(),
                "test".into(),
                "./...".into(),
                format!("-coverprofile={}", profile.display()),
            ]),
            &module,
        );
        if code != 0 {
            warn(&format!(
                "Go coverage exited {code} in {}. Go coverage will score 0%.",
                module.display()
            ));
        }
    }
}

fn needs_vitest_provider(command: &[String]) -> bool {
    if command.len() >= 3 && command[0] == "npm" && command[1] == "run" && command[2] == "coverage"
    {
        return false;
    }
    command
        .iter()
        .any(|part| Path::new(part).file_name().and_then(|n| n.to_str()) == Some("vitest"))
}

fn cover_typescript(root: &Path, files: &[PathBuf]) {
    let packages = typescript_packages(root, files);
    if packages.is_empty() {
        warn("No package.json; skipping TypeScript coverage.");
        return;
    }
    for package in packages {
        let report = coverage_report(root, &package, "typescript");
        let report_dir = report.parent().unwrap_or(root);
        let command = match typescript_command(&package, files, report_dir) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => {
                warn(&format!(
                    "No package.json test script in {}; skipping TypeScript coverage.",
                    package.display()
                ));
                continue;
            }
            Err(exc) => {
                warn(&exc.0);
                continue;
            }
        };
        if needs_vitest_provider(&command) && !ensure_vitest_coverage(&package) {
            continue;
        }
        clean_dir(report_dir);
        let _ = fs::create_dir_all(report_dir);
        let code = run_shell(&ShellCommand::Args(command), &package);
        if code != 0 {
            warn(&format!(
                "TypeScript coverage exited {code} in {}. TypeScript coverage will score 0%.",
                package.display()
            ));
        }
    }
}

fn python_kind(py_exe: &str, package: &Path) -> &'static str {
    if !uses_pytest(package) {
        return "unittest";
    }
    if ensure_python_module(py_exe, package, "pytest") {
        return "pytest";
    }
    warn(&format!(
        "pytest is missing in {}. Falling back to unittest.",
        package.display()
    ));
    "unittest"
}

fn cover_python(root: &Path, files: &[PathBuf]) {
    for package in python_roots(root, files) {
        let report = coverage_report(root, &package, "python");
        let py_exe = python_executable(&package);
        if !ensure_python_module(&py_exe, &package, "coverage") {
            warn(&format!(
                "coverage is missing in {}. Python coverage will score 0%.",
                package.display()
            ));
            continue;
        }
        let report_dir = report.parent().unwrap_or(root);
        let _ = fs::create_dir_all(report_dir);
        let data_file = report_dir.join(".coverage");
        remove_file(&report);
        remove_file(&data_file);
        let kind = python_kind(&py_exe, &package);
        let (run_cmd, lcov_cmd) = python_coverage_commands(
            &py_exe,
            kind,
            &data_file,
            &report,
            &python_sources(&package, files),
        );
        let code = run_shell(&ShellCommand::Args(run_cmd), &package);
        if !data_file.exists() {
            if code != 0 {
                warn(&format!(
                    "Python coverage exited {code} in {}. Python coverage will score 0%.",
                    package.display()
                ));
            }
            continue;
        }
        let lcov_code = run_shell(&ShellCommand::Args(lcov_cmd), &package);
        if lcov_code != 0 {
            warn(&format!(
                "coverage lcov exited {lcov_code} in {}. Python coverage will score 0%.",
                package.display()
            ));
        } else if code != 0 {
            warn(&format!(
                "Python tests exited {code} in {}. Coverage was still recorded.",
                package.display()
            ));
        }
    }
}

fn cover_rust(root: &Path, files: &[PathBuf]) {
    let modules = rust_modules(root, files);
    if modules.is_empty() {
        warn("No Cargo.toml; skipping Rust coverage.");
        return;
    }
    let Some(kind) = rust_kind() else {
        return;
    };
    for module in modules {
        let report = coverage_report(root, &module, "rust");
        if let Some(parent) = report.parent() {
            let _ = fs::create_dir_all(parent);
        }
        remove_file(&report);
        let code = run_shell(
            &ShellCommand::Args(rust_coverage_command(kind, &report)),
            &module,
        );
        if code != 0 {
            warn(&format!(
                "Rust coverage exited {code} in {}. Rust coverage will score 0%.",
                module.display()
            ));
        }
    }
}

fn cover_swift(root: &Path, files: &[PathBuf]) {
    let packages = swift_packages(root, files);
    for package in &packages {
        let report = coverage_report(root, package, "swift").with_file_name("llvm-cov.json");
        remove_file(&report);
        if which_program("swift").is_none() {
            warn(&format!(
                "swift is not installed. Swift coverage will score 0% in {}.",
                package.display()
            ));
            continue;
        }
        let code = run_shell(&ShellCommand::Args(swift_test_command()), package);
        if code != 0 {
            warn(&format!(
                "Swift coverage exited {code} in {}. Swift coverage will score 0%.",
                package.display()
            ));
            continue;
        }
        let (Some(profdata), Some(binary), Some(tool)) = (
            swift_profdata(package),
            swift_test_binary(package),
            llvm_cov_tool(),
        ) else {
            remove_file(&report);
            warn(&format!(
                "Swift coverage data is missing in {}. Swift coverage will score 0%.",
                package.display()
            ));
            continue;
        };
        let exp_code = write_command_output(
            &swift_export_command(&tool, &binary, &profdata),
            package,
            &report,
        );
        if exp_code != 0 {
            remove_file(&report);
            warn(&format!(
                "llvm-cov export exited {exp_code} in {}. Swift coverage will score 0%.",
                package.display()
            ));
        }
    }

    let outside: Vec<PathBuf> = files
        .iter()
        .filter(|f| crate::discover::path_suffix(f) == Some(".swift"))
        .filter(|f| {
            let rf = resolve_path(f);
            !packages.iter().any(|p| rf.starts_with(resolve_path(p)))
        })
        .cloned()
        .collect();
    for project in xcode_projects(root, &outside) {
        let report_dir = coverage_report(root, &project, "swift");
        let parent = report_dir.parent().unwrap_or(root);
        let exported = parent.join("xccov.json");
        remove_file(&exported);
        if which_program("xcodebuild").is_none() {
            let name = project.file_name().and_then(|n| n.to_str()).unwrap_or("");
            warn(&format!(
                "xcodebuild is not installed. Swift coverage will score 0% for {name}."
            ));
            continue;
        }
        let schemes = xcode_schemes(&project);
        if schemes.is_empty() {
            warn(&format!(
                "No shared Xcode scheme in {}. Swift coverage will score 0%.",
                project.display()
            ));
            continue;
        }
        let result = parent.join("Test.xcresult");
        clean_dir(&result);
        let cwd = project.parent().unwrap_or(root);
        let code = run_shell(
            &ShellCommand::Args(xcode_test_command(&project, &schemes[0], &result)),
            cwd,
        );
        if code != 0 {
            warn(&format!(
                "Xcode coverage exited {code} in {}. Swift coverage will score 0%.",
                project.display()
            ));
            continue;
        }
        let exp_code = write_command_output(&xcode_export_command(&result), cwd, &exported);
        if exp_code != 0 {
            remove_file(&exported);
            warn(&format!(
                "xccov exited {exp_code} in {}. Swift coverage will score 0%.",
                project.display()
            ));
        }
    }
}

pub fn run_coverage(root: &Path, files: &[PathBuf], command: Option<&str>) -> i32 {
    let root = resolve_path(root);
    if let Some(cmd) = command.filter(|s| !s.is_empty()) {
        let code = run_shell(&ShellCommand::Shell(cmd.to_string()), &root);
        if code != 0 {
            warn(&format!(
                "Coverage command exited {code}. Reports already on disk will not be read."
            ));
        }
        return code;
    }

    let mut present: BTreeSet<&str> = files.iter().filter_map(|f| language_of(f)).collect();
    if present.contains("vue") {
        present.insert("typescript");
    }
    for lang in [
        "clojure",
        "java",
        "go",
        "typescript",
        "python",
        "rust",
        "swift",
    ] {
        if !present.contains(lang) {
            continue;
        }
        match lang {
            "clojure" => cover_clojure(&root),
            "java" => cover_java(&root, files),
            "go" => cover_go(&root, files),
            "typescript" => cover_typescript(&root, files),
            "python" => cover_python(&root, files),
            "rust" => cover_rust(&root, files),
            "swift" => cover_swift(&root, files),
            _ => {}
        }
    }
    0
}
