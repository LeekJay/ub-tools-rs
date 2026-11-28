//! Run a project's tests, and decide the command for each language.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use crate::path_util::{relative_posix, resolve_path};

/// Either a shell command string (`--test-command`) or an argument vector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Command {
    Shell(String),
    Argv(Vec<String>),
}

/// Output and status from running one command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandResult {
    pub code: i32,
    pub timed_out: bool,
    pub seconds: f64,
    pub output: String,
}

fn worker_home(cwd: &Path) -> Option<PathBuf> {
    for candidate in cwd.ancestors() {
        let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("worker-") {
            continue;
        }
        let Some(parent) = candidate.parent() else {
            continue;
        };
        let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !parent_name.starts_with("run-") {
            continue;
        }
        if parent
            .parent()
            .and_then(|gp| gp.file_name())
            .and_then(|n| n.to_str())
            == Some("mutation-workers")
        {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn source_entries(cwd: &Path, worker: &Path) -> Vec<OsString> {
    let candidates = [
        cwd.join("src"),
        cwd.to_path_buf(),
        worker.join("src"),
        worker.to_path_buf(),
    ];
    let mut found = Vec::new();
    for directory in candidates {
        let os = directory.as_os_str().to_os_string();
        if found.contains(&os) {
            continue;
        }
        if directory.is_dir() {
            found.push(os);
        }
    }
    found
}

fn prefer_worker_sources(cwd: &Path, environment: &mut HashMap<OsString, OsString>) {
    let Some(worker) = worker_home(cwd) else {
        return;
    };
    let mut entries = source_entries(cwd, &worker);
    if entries.is_empty() {
        return;
    }
    if let Some(current) = environment.get(std::ffi::OsStr::new("PYTHONPATH"))
        && !current.is_empty()
    {
        entries.push(current.clone());
    }
    if let Ok(joined) = std::env::join_paths(entries) {
        environment.insert(OsString::from("PYTHONPATH"), joined);
    }
}

/// Render a command for display; argument vectors are POSIX shell-quoted.
pub fn display_command(command: &Command) -> String {
    match command {
        Command::Shell(s) => s.clone(),
        Command::Argv(parts) => parts
            .iter()
            .map(|part| shlex_quote(part))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn shlex_quote(part: &str) -> String {
    if part.is_empty() {
        return "''".to_string();
    }
    let safe = part.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    });
    if safe {
        return part.to_string();
    }
    format!("'{}'", part.replace('\'', "'\"'\"'"))
}

fn apply_process_group(cmd: &mut ProcessCommand) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(ub_runtime::child_creation_flags(CREATE_NEW_PROCESS_GROUP));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

fn terminate_process_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        let _ = ProcessCommand::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
    }
    #[cfg(unix)]
    {
        let _ = ProcessCommand::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
    }
}

fn build_process_command(command: &Command) -> ProcessCommand {
    match command {
        Command::Argv(args) => {
            if args.is_empty() {
                return ProcessCommand::new("");
            }
            let mut proc = ProcessCommand::new(&args[0]);
            proc.args(&args[1..]);
            proc
        }
        Command::Shell(cmd) => {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let comspec = std::env::var_os("ComSpec")
                    .filter(|v| !v.is_empty())
                    .or_else(|| {
                        std::env::var_os("SystemRoot")
                            .map(|root| PathBuf::from(root).join("System32").join("cmd.exe").into())
                    })
                    .unwrap_or_else(|| OsString::from("cmd.exe"));
                let mut proc = ProcessCommand::new(comspec);
                proc.raw_arg(format!("/c \"{cmd}\""));
                proc
            }
            #[cfg(not(windows))]
            {
                let mut proc = ProcessCommand::new("/bin/sh");
                proc.arg("-c").arg(cmd);
                proc
            }
        }
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Option<f64>) -> (i32, bool) {
    let Some(secs) = timeout else {
        let code = child
            .wait()
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(1);
        return (code, false);
    };

    let deadline = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
    let mut sleep_ms = 1u64;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(1), false),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    terminate_process_tree(child);
                    let _ = child.wait();
                    return (124, true);
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(remaining.min(Duration::from_millis(sleep_ms)));
                sleep_ms = (sleep_ms * 2).min(15);
            }
            Err(_) => {
                let _ = child.wait();
                return (1, false);
            }
        }
    }
}

/// Trait abstracting test execution for `workers` and `engine` unit testing.
pub trait Runner {
    fn is_verbose(&self) -> bool;
    fn run(&self, command: &Command, cwd: &Path, timeout: Option<f64>) -> CommandResult;
}

/// Real subprocess runner with process-tree timeout killing.
#[derive(Debug, Clone, Default)]
pub struct CommandRunner {
    pub verbose: bool,
}

impl CommandRunner {
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }
}

impl Runner for CommandRunner {
    fn is_verbose(&self) -> bool {
        self.verbose
    }

    fn run(&self, command: &Command, cwd: &Path, timeout: Option<f64>) -> CommandResult {
        if self.verbose {
            eprintln!("+ ({}) {}", cwd.display(), display_command(command));
        }
        let started = Instant::now();
        let mut environment: HashMap<OsString, OsString> = std::env::vars_os().collect();
        environment.insert(
            OsString::from("PYTHONDONTWRITEBYTECODE"),
            OsString::from("1"),
        );
        prefer_worker_sources(cwd, &mut environment);

        let Ok((mut reader, writer)) = std::io::pipe() else {
            return CommandResult {
                code: 127,
                timed_out: false,
                seconds: started.elapsed().as_secs_f64(),
                output: "failed to create output pipe".to_string(),
            };
        };
        let Ok(writer_err) = writer.try_clone() else {
            return CommandResult {
                code: 127,
                timed_out: false,
                seconds: started.elapsed().as_secs_f64(),
                output: "failed to clone pipe writer".to_string(),
            };
        };

        let mut proc = build_process_command(command);
        proc.current_dir(cwd)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer_err));
        apply_process_group(&mut proc);

        let mut child = match proc.spawn() {
            Ok(child) => child,
            Err(exc) => {
                return CommandResult {
                    code: 127,
                    timed_out: false,
                    seconds: started.elapsed().as_secs_f64(),
                    output: exc.to_string(),
                };
            }
        };
        // Drop parent's pipe write ends by dropping `proc` so EOF arrives once child exits.
        drop(proc);

        let reader_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        });

        let (code, timed_out) = wait_with_timeout(&mut child, timeout);
        let bytes = reader_thread.join().unwrap_or_default();
        let output = String::from_utf8_lossy(&bytes).into_owned();
        CommandResult {
            code,
            timed_out,
            seconds: started.elapsed().as_secs_f64(),
            output,
        }
    }
}

/// Walk upward from `start` until a directory containing `marker` is found, stopping at `stop`.
pub fn nearest(start: &Path, marker: &str, stop: &Path) -> Option<PathBuf> {
    let base = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(start)
    };
    let stop_resolved = resolve_path(stop);
    let mut current = resolve_path(base);
    loop {
        if current.join(marker).is_file() {
            return Some(current);
        }
        if current == stop_resolved {
            return None;
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Determine Clojure test command in `directory`.
pub fn clojure_command(directory: &Path) -> Vec<String> {
    let deps = directory.join("deps.edn");
    let bb = directory.join("bb.edn");
    if bb.is_file() && !deps.is_file() {
        let text = read_text(&bb);
        if text.contains("spec") {
            return vec!["bb", "spec", "--tag", "~no-mutate"]
                .into_iter()
                .map(str::to_string)
                .collect();
        }
        return vec!["bb", "test"].into_iter().map(str::to_string).collect();
    }
    let text = read_text(&deps);
    if text.contains(":spec") && text.contains("speclj") {
        return vec!["clj", "-M:spec", "--tag", "~no-mutate"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    if text.contains(":spec") {
        return vec!["clj", "-M:spec"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    vec!["clj", "-M:test"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn interpreter_path(directory: &Path, name: &str, windows: bool) -> PathBuf {
    let path = if windows {
        directory.join(name).join("Scripts").join("python.exe")
    } else {
        directory.join(name).join("bin").join("python")
    };
    std::path::absolute(&path).unwrap_or(path)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn fallback_python() -> String {
    #[cfg(windows)]
    {
        "python".to_string()
    }
    #[cfg(not(windows))]
    {
        "python3".to_string()
    }
}

fn project_interpreter(directory: &Path) -> String {
    let windows = cfg!(windows);
    for name in [".venv", "venv"] {
        let candidate = interpreter_path(directory, name, windows);
        if is_executable_file(&candidate) {
            return candidate.to_string_lossy().into_owned();
        }
    }
    fallback_python()
}

/// Determine Python test command in `directory`, preferring `.venv`/`venv` Python.
pub fn python_command(directory: &Path) -> Vec<String> {
    let interpreter = project_interpreter(directory);
    let pyproject = read_text(&directory.join("pyproject.toml"));
    let has_pytest = directory.join("pytest.ini").is_file()
        || directory.join("conftest.py").is_file()
        || pyproject.contains("pytest")
        || (directory.join("setup.cfg").is_file()
            && read_text(&directory.join("setup.cfg")).contains("pytest"));
    if has_pytest {
        vec![interpreter, "-m".into(), "pytest".into()]
    } else {
        vec![
            interpreter,
            "-m".into(),
            "unittest".into(),
            "discover".into(),
        ]
    }
}

fn nearest_marker(source: &Path, root: &Path, markers: &[&str]) -> PathBuf {
    for marker in markers {
        if let Some(found) = nearest(source, marker, root) {
            return found;
        }
    }
    root.to_path_buf()
}

fn go_package(directory: &Path, source: &Path) -> String {
    let parent = source.parent().unwrap_or(source);
    let Some(rel) = relative_posix(parent, directory) else {
        return ".".to_string();
    };
    if rel.is_empty() || rel == "." {
        ".".to_string()
    } else {
        format!("./{rel}")
    }
}

/// Compute the test command and working directory for a source file.
///
/// Swift projects delegate discovery to `xcode_finder` (`crapper::runners`).
pub fn test_plan_with<F>(
    root: &Path,
    source: &Path,
    language: &str,
    override_cmd: Option<&str>,
    xcode_scheme: Option<&str>,
    xcode_destination: Option<&str>,
    swift_resolver: F,
) -> (Command, PathBuf)
where
    F: FnOnce(&Path, &Path, Option<&str>, Option<&str>) -> (Command, PathBuf),
{
    let root = resolve_path(root);
    if let Some(cmd) = override_cmd
        && !cmd.is_empty()
    {
        return (Command::Shell(cmd.to_string()), root);
    }
    if language == "swift" {
        return swift_resolver(
            &root,
            &resolve_path(source),
            xcode_scheme,
            xcode_destination,
        );
    }
    match language {
        "clojure" => {
            let dir = nearest_marker(source, &root, &["deps.edn", "bb.edn"]);
            (Command::Argv(clojure_command(&dir)), dir)
        }
        "java" => {
            let dir = nearest_marker(source, &root, &["pom.xml"]);
            (
                Command::Argv(
                    ["mvn", "-q", "test", "-DexcludeTags=no-mutate"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                ),
                dir,
            )
        }
        "go" => {
            let dir = nearest_marker(source, &root, &["go.mod"]);
            let pkg = go_package(&dir, source);
            (
                Command::Argv(vec!["go".into(), "test".into(), "-count=1".into(), pkg]),
                dir,
            )
        }
        "typescript" => {
            let dir = nearest_marker(source, &root, &["package.json"]);
            (Command::Argv(vec!["npm".into(), "test".into()]), dir)
        }
        "rust" => {
            let dir = nearest_marker(source, &root, &["Cargo.toml"]);
            (Command::Argv(vec!["cargo".into(), "test".into()]), dir)
        }
        "python" => {
            let dir = nearest_marker(
                source,
                &root,
                &["pyproject.toml", "pytest.ini", "setup.cfg"],
            );
            (Command::Argv(python_command(&dir)), dir)
        }
        _ => (Command::Argv(vec!["false".into()]), root),
    }
}

/// Helper building an `xcodebuild` command once Swift package/project resolution is available.
pub fn xcode_command(
    project: &Path,
    scheme: Option<&str>,
    destination: Option<&str>,
    schemes: &[String],
) -> Vec<String> {
    let flag = if project.extension().and_then(|e| e.to_str()) == Some("xcworkspace") {
        "-workspace"
    } else {
        "-project"
    };
    let chosen_scheme = match scheme {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => schemes.first().cloned().unwrap_or_else(|| {
            project
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        }),
    };
    let mut cmd = vec![
        "xcodebuild".to_string(),
        "test".to_string(),
        flag.to_string(),
        project
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
        "-scheme".to_string(),
        chosen_scheme,
    ];
    if let Some(dest) = destination
        && !dest.is_empty()
    {
        cmd.push("-destination".to_string());
        cmd.push(dest.to_string());
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mutator_runner_test_{name}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nearest_walks_up_to_marker() {
        let root = temp_dir("nearest");
        let nested = root.join("src").join("demo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let source = nested.join("app.py");
        std::fs::write(&source, "x = 1\n").unwrap();

        assert_eq!(
            nearest(&source, "pyproject.toml", &root),
            Some(resolve_path(&root))
        );
        assert_eq!(
            nearest(&nested, "pyproject.toml", &root),
            Some(resolve_path(&root))
        );
        assert_eq!(nearest(&source, "missing.toml", &root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clojure_and_python_commands_match() {
        let root = temp_dir("cmds");
        let bb = root.join("bb-spec");
        std::fs::create_dir_all(&bb).unwrap();
        std::fs::write(bb.join("bb.edn"), "{:tasks {spec \"spec\"}}").unwrap();
        assert_eq!(
            clojure_command(&bb),
            vec!["bb", "spec", "--tag", "~no-mutate"]
        );

        let spec = root.join("spec");
        std::fs::create_dir_all(&spec).unwrap();
        std::fs::write(
            spec.join("deps.edn"),
            "{:aliases {:spec {:extra-deps {speclj/speclj {}}}}}",
        )
        .unwrap();
        assert_eq!(
            clojure_command(&spec),
            vec!["clj", "-M:spec", "--tag", "~no-mutate"]
        );

        let py = root.join("py");
        std::fs::create_dir_all(&py).unwrap();
        let venv_py = if cfg!(windows) {
            py.join(".venv").join("Scripts").join("python.exe")
        } else {
            py.join(".venv").join("bin").join("python")
        };
        std::fs::create_dir_all(venv_py.parent().unwrap()).unwrap();
        std::fs::write(&venv_py, "").unwrap();
        std::fs::write(py.join("pytest.ini"), "[pytest]\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&venv_py, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cmd = python_command(&py);
        assert_eq!(
            cmd,
            vec![
                std::path::absolute(&venv_py)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                "-m".into(),
                "pytest".into()
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn command_runner_kills_timed_out_process_tree() {
        let worker = temp_dir("timeout");
        let child_script = "import subprocess, sys; proc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); proc.wait()";
        let result = CommandRunner::new(false).run(
            &Command::Argv(vec![fallback_python(), "-c".into(), child_script.into()]),
            &worker,
            Some(0.3),
        );
        assert!(result.timed_out);
        assert_eq!(result.code, 124);
        assert!(
            result.seconds < 15.0,
            "timed out process tree took {:.2}s (30s grandchild may have survived)",
            result.seconds
        );
        let _ = std::fs::remove_dir_all(&worker);
    }

    #[test]
    fn worker_overlay_sets_pythonpath_ahead_of_environment() {
        let root = temp_dir("worker_py");
        let worker = root
            .join("target")
            .join("mutation-workers")
            .join("run-1")
            .join("worker-0");
        std::fs::create_dir_all(worker.join("src")).unwrap();
        std::fs::write(worker.join("src").join("demo.py"), "VALUE = 'worker'\n").unwrap();
        let cmd = Command::Shell(format!(
            "{} -c \"import demo; print(demo.VALUE)\"",
            fallback_python()
        ));
        let res = CommandRunner::new(false).run(&cmd, &worker, Some(5.0));
        assert_eq!(res.code, 0, "output: {}", res.output);
        assert_eq!(res.output.trim(), "worker");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_routes_all_non_swift_languages() {
        let root = temp_dir("plan");
        let sub = root.join("svc");
        std::fs::create_dir_all(sub.join("pkg")).unwrap();
        std::fs::write(sub.join("go.mod"), "module example.com/demo\n").unwrap();
        let go_file = sub.join("pkg").join("widget.go");
        std::fs::write(&go_file, "package pkg\n").unwrap();

        let (cmd, dir) = test_plan_with(
            &root,
            &go_file,
            "go",
            None,
            None,
            None,
            |_, _, _, _| unreachable!(),
        );
        assert_eq!(
            cmd,
            Command::Argv(vec![
                "go".into(),
                "test".into(),
                "-count=1".into(),
                "./pkg".into()
            ])
        );
        assert_eq!(dir, resolve_path(&sub));

        let (ovr, ovr_dir) = test_plan_with(
            &root,
            &go_file,
            "go",
            Some("go test ./..."),
            None,
            None,
            |_, _, _, _| unreachable!(),
        );
        assert_eq!(ovr, Command::Shell("go test ./...".into()));
        assert_eq!(ovr_dir, resolve_path(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}
