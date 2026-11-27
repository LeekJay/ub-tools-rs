#![deny(unsafe_code)]

mod atomic;
mod cache;
mod git;
mod lock;
mod priority;
mod runtime;

pub use atomic::atomic_write;
pub use cache::{
    CacheDecoder, CacheEncoder, FileStamp, ManifestStampCache, cache_dir, env_no_cache,
    file_mtime_size, load_cache_file, repo_cache_path, save_cache_file,
};
pub use git::{ChangedLines, GitCommandError, git_changed_lines, parse_diff_line_ranges, run_git};
pub use lock::{
    LockGuard, acquire_global_scan_slot, acquire_global_scan_slot_in, acquire_repo_lock,
    acquire_repo_lock_in, acquire_repo_shared_lock, acquire_repo_shared_lock_in,
};
pub use priority::{apply_child_priority, child_creation_flags};
pub use runtime::{
    default_mutator_workers, env_no_metrics, init_cli_runtime, resolve_rayon_threads,
};

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const SUBPROCESS_MARKER: &str = "UB_RUNTIME_TEST_SUBPROC";

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
    fn env_var_public_api_via_isolated_subprocesses() {
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            assert_eq!(default_mutator_workers(16), 3);
            assert!(env_no_metrics());
            assert!(
                acquire_global_scan_slot()
                    .expect("max concurrent 0")
                    .is_none()
            );

            #[cfg(windows)]
            {
                assert_eq!(child_creation_flags(0), 0x0000_0040); // IDLE_PRIORITY_CLASS
                assert_eq!(child_creation_flags(0x0200), 0x0240);
            }
            #[cfg(not(windows))]
            {
                assert_eq!(child_creation_flags(0x0200), 0x0200);
            }

            init_cli_runtime();
            assert_eq!(rayon::current_num_threads(), 3);
            return;
        }

        run_self_test(
            "tests::env_var_public_api_via_isolated_subprocesses",
            &[
                ("UB_TOOLS_MUTATOR_WORKERS", Some("3")),
                ("UB_TOOLS_NO_METRICS", Some(" YES ")),
                ("UB_TOOLS_MAX_CONCURRENT", Some("0")),
                ("UB_TOOLS_PRIORITY", Some("idle")),
                ("UB_TOOLS_THREADS", Some("3")),
                ("RAYON_NUM_THREADS", Some("7")),
            ],
        );
    }

    #[test]
    fn init_cli_runtime_respects_rayon_num_threads_when_ub_threads_unset() {
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            assert!(!env_no_metrics());
            assert_eq!(child_creation_flags(0x0200), 0x0200);
            init_cli_runtime();
            assert_eq!(rayon::current_num_threads(), 5);
            return;
        }

        run_self_test(
            "tests::init_cli_runtime_respects_rayon_num_threads_when_ub_threads_unset",
            &[
                ("UB_TOOLS_NO_METRICS", Some("false")),
                ("UB_TOOLS_PRIORITY", Some("NORMAL")),
                ("UB_TOOLS_THREADS", None),
                ("RAYON_NUM_THREADS", Some("5")),
            ],
        );
    }

    #[test]
    fn init_cli_runtime_uses_resolve_rayon_threads_by_default() {
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            init_cli_runtime();
            let cores = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1);
            assert_eq!(rayon::current_num_threads(), resolve_rayon_threads(cores));
            return;
        }

        run_self_test(
            "tests::init_cli_runtime_uses_resolve_rayon_threads_by_default",
            &[
                ("UB_TOOLS_THREADS", None),
                ("RAYON_NUM_THREADS", None),
                ("UB_TOOLS_PRIORITY", None),
            ],
        );
    }

    #[test]
    fn apply_child_priority_spawns_cleanly() {
        let exe = std::env::current_exe().expect("current test exe");
        let mut cmd = Command::new(exe);
        cmd.arg("--list");
        apply_child_priority(&mut cmd);
        let output = cmd.output().expect("spawn with apply_child_priority");
        assert!(output.status.success());
    }

    #[test]
    fn repo_lock_prints_wait_notice_to_stderr_when_contended() {
        const NOTICE_DIR_ENV: &str = "UB_RUNTIME_TEST_LOCK_DIR";
        const NOTICE_REPO_ENV: &str = "UB_RUNTIME_TEST_LOCK_REPO";

        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            let dir = std::path::PathBuf::from(std::env::var_os(NOTICE_DIR_ENV).expect("dir env"));
            let repo =
                std::path::PathBuf::from(std::env::var_os(NOTICE_REPO_ENV).expect("repo env"));
            let _guard = acquire_repo_lock_in(&dir, "crap", &repo, Some("waiting on repo lock"))
                .expect("acquire contended lock");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "ub-runtime-notice-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        let guard = acquire_repo_lock_in(&dir, "crap", &repo, None).expect("initial lock");

        let exe = std::env::current_exe().expect("current test exe");
        let child = Command::new(exe)
            .args([
                "--exact",
                "tests::repo_lock_prints_wait_notice_to_stderr_when_contended",
                "--nocapture",
            ])
            .env(SUBPROCESS_MARKER, "1")
            .env(NOTICE_DIR_ENV, &dir)
            .env(NOTICE_REPO_ENV, &repo)
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock waiter");

        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(guard);

        let output = child.wait_with_output().expect("wait child");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("waiting on repo lock"),
            "expected wait notice on stderr, got: {stderr}"
        );
    }

    #[test]
    fn default_lock_wrappers_acquire_and_release() {
        let unique_kind = format!(
            "ubrt-smoke-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let root = std::env::temp_dir().join(&unique_kind);
        std::fs::create_dir_all(&root).expect("create temp repo root");

        let excl = acquire_repo_lock(&unique_kind, &root, None).expect("default repo lock");
        drop(excl);

        let shared1 =
            acquire_repo_shared_lock(&unique_kind, &root).expect("default repo shared lock 1");
        let shared2 =
            acquire_repo_shared_lock(&unique_kind, &root).expect("default repo shared lock 2");
        drop(shared1);
        drop(shared2);

        let hash = lock::repo_hash(&root);
        let lock_file = std::env::temp_dir()
            .join("ub-tools-locks")
            .join(format!("{unique_kind}-{hash}.lock"));
        let _ = std::fs::remove_file(lock_file);
        let _ = std::fs::remove_dir_all(&root);
    }
}
