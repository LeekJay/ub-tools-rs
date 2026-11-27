use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

const ENV_MAX_CONCURRENT: &str = "UB_TOOLS_MAX_CONCURRENT";
const DEFAULT_MAX_CONCURRENT: usize = 2;
const LOCK_DIR_NAME: &str = "ub-tools-locks";
const REPO_HASH_BYTES: usize = 8;
const SCAN_SLOT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// RAII guard holding an OS file lock (`std::fs::File`), released on `Drop`.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn default_lock_dir() -> PathBuf {
    std::env::temp_dir().join(LOCK_DIR_NAME)
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn normalize_repo_path(root: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(root) {
        return strip_verbatim(canon);
    }
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    strip_verbatim(lexical_normalize(&abs))
}

pub(crate) fn repo_hash(root: &Path) -> String {
    let normalized = normalize_repo_path(root);
    let raw = normalized.to_string_lossy();
    #[cfg(windows)]
    let key = raw.to_lowercase();
    #[cfg(not(windows))]
    let key = raw.as_ref();

    let digest = Sha256::digest(key.as_bytes());
    let mut hex = String::with_capacity(REPO_HASH_BYTES * 2);
    for byte in &digest[..REPO_HASH_BYTES] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn sanitize_kind(kind: &str) -> String {
    let sanitized: String = kind
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "repo".to_string()
    } else {
        sanitized
    }
}

fn repo_lock_path(dir: &Path, kind: &str, root: &Path) -> PathBuf {
    let safe_kind = sanitize_kind(kind);
    let hash = repo_hash(root);
    dir.join(format!("{safe_kind}-{hash}.lock"))
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Acquires an exclusive lock for `(kind, root)` in the default lock directory.
///
/// Calls `try_lock()` first; if `WouldBlock`, prints `wait_notice` to stderr when `Some`,
/// then blocks until `lock()` succeeds.
pub fn acquire_repo_lock(
    kind: &str,
    root: &Path,
    wait_notice: Option<&str>,
) -> io::Result<LockGuard> {
    acquire_repo_lock_in(&default_lock_dir(), kind, root, wait_notice)
}

/// Acquires an exclusive lock for `(kind, root)` inside `dir`.
pub fn acquire_repo_lock_in(
    dir: &Path,
    kind: &str,
    root: &Path,
    wait_notice: Option<&str>,
) -> io::Result<LockGuard> {
    fs::create_dir_all(dir)?;
    let lock_path = repo_lock_path(dir, kind, root);
    let file = open_lock_file(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(LockGuard { file }),
        Err(TryLockError::WouldBlock) => {
            if let Some(notice) = wait_notice {
                eprintln!("{notice}");
            }
            file.lock()?;
            Ok(LockGuard { file })
        }
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Acquires a shared lock for `(kind, root)` in the default lock directory.
pub fn acquire_repo_shared_lock(kind: &str, root: &Path) -> io::Result<LockGuard> {
    acquire_repo_shared_lock_in(&default_lock_dir(), kind, root)
}

/// Acquires a shared lock for `(kind, root)` inside `dir`.
pub fn acquire_repo_shared_lock_in(dir: &Path, kind: &str, root: &Path) -> io::Result<LockGuard> {
    fs::create_dir_all(dir)?;
    let lock_path = repo_lock_path(dir, kind, root);
    let file = open_lock_file(&lock_path)?;
    file.lock_shared()?;
    Ok(LockGuard { file })
}

pub(crate) fn parse_max_concurrent(raw: Option<&str>) -> usize {
    let Some(value) = raw else {
        return DEFAULT_MAX_CONCURRENT;
    };
    let trimmed = value.trim();
    if trimmed == "0" {
        return 0;
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n > 0 => n,
        _ => DEFAULT_MAX_CONCURRENT,
    }
}

/// Acquires one of the global scan semaphore slots configured by `UB_TOOLS_MAX_CONCURRENT`.
///
/// `"0"` disables throttling and returns `Ok(None)`; any positive `usize` sets the slot count
/// (default `2`).
pub fn acquire_global_scan_slot() -> io::Result<Option<LockGuard>> {
    let raw = std::env::var(ENV_MAX_CONCURRENT).ok();
    let slots = parse_max_concurrent(raw.as_deref());
    acquire_global_scan_slot_in(&default_lock_dir(), slots)
}

/// Acquires one of `max_slots` global scan slots inside `dir`.
///
/// Returns `Ok(None)` when `max_slots == 0`. Otherwise sweeps `global-scan-slot-{i}.lock`
/// starting at `pid % max_slots` with `try_lock()`, sleeping 25ms between full sweeps.
pub fn acquire_global_scan_slot_in(dir: &Path, max_slots: usize) -> io::Result<Option<LockGuard>> {
    if max_slots == 0 {
        return Ok(None);
    }
    fs::create_dir_all(dir)?;

    let pid = usize::try_from(std::process::id()).unwrap_or(0);
    let start = pid % max_slots;
    loop {
        for offset in 0..max_slots {
            let slot = (start + offset) % max_slots;
            let lock_path = dir.join(format!("global-scan-slot-{slot}.lock"));
            let file = open_lock_file(&lock_path)?;
            match file.try_lock() {
                Ok(()) => return Ok(Some(LockGuard { file })),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(err)) => return Err(err),
            }
        }
        std::thread::sleep(SCAN_SLOT_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    use uuid::Uuid;

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ub-runtime-lock-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).expect("create lock test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn repo_hash_normalizes_paths_and_is_16_hex_chars() {
        let dir = TestDir::new("hash");
        let sub = dir.0.join("repo");
        fs::create_dir_all(&sub).expect("create repo dir");

        let direct = repo_hash(&sub);
        let dot_dot = repo_hash(&sub.join("nested").join(".."));
        assert_eq!(direct.len(), 16);
        assert!(direct.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(direct, dot_dot);

        #[cfg(windows)]
        {
            let upper = PathBuf::from(sub.to_string_lossy().to_uppercase());
            assert_eq!(direct, repo_hash(&upper));
        }

        let other = repo_hash(&dir.0.join("other-repo"));
        assert_ne!(direct, other);
    }

    #[test]
    fn parses_max_concurrent_values() {
        assert_eq!(parse_max_concurrent(None), 2);
        assert_eq!(parse_max_concurrent(Some("")), 2);
        assert_eq!(parse_max_concurrent(Some("0")), 0);
        assert_eq!(parse_max_concurrent(Some(" 0 ")), 0);
        assert_eq!(parse_max_concurrent(Some("4")), 4);
        assert_eq!(parse_max_concurrent(Some(" 1 ")), 1);
        assert_eq!(parse_max_concurrent(Some("-3")), 2);
        assert_eq!(parse_max_concurrent(Some("bad")), 2);
    }

    #[test]
    fn exclusive_lock_blocks_second_exclusive_holder_until_dropped() {
        let lock_dir = TestDir::new("excl");
        let repo = lock_dir.0.join("repo");

        let guard = acquire_repo_lock_in(&lock_dir.0, "crap", &repo, None).expect("first lock");
        let (tx, rx) = mpsc::channel();
        let dir_clone = lock_dir.0.clone();
        let repo_clone = repo.clone();

        let waiter = std::thread::spawn(move || {
            let _guard2 = acquire_repo_lock_in(
                &dir_clone,
                "crap",
                &repo_clone,
                Some("waiting for test lock"),
            )
            .expect("second lock");
            tx.send("acquired").expect("send acquired");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "second exclusive lock acquired while first was still held"
        );
        drop(guard);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("acquired"),
            "acquired"
        );
        waiter.join().expect("waiter join");
    }

    #[test]
    fn shared_locks_allow_concurrency_and_block_exclusive_lock() {
        let lock_dir = TestDir::new("shared");
        let repo = lock_dir.0.join("repo");

        let s1 = acquire_repo_shared_lock_in(&lock_dir.0, "crap", &repo).expect("shared 1");
        let s2 = acquire_repo_shared_lock_in(&lock_dir.0, "crap", &repo).expect("shared 2");

        let (tx, rx) = mpsc::channel();
        let dir_clone = lock_dir.0.clone();
        let repo_clone = repo.clone();
        let excl_waiter = std::thread::spawn(move || {
            let _excl =
                acquire_repo_lock_in(&dir_clone, "crap", &repo_clone, None).expect("excl lock");
            tx.send("exclusive").expect("send excl");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "exclusive lock acquired while shared locks held"
        );
        drop(s1);
        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "exclusive lock acquired while second shared lock still held"
        );
        drop(s2);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("excl done"),
            "exclusive"
        );
        excl_waiter.join().expect("excl waiter join");
    }

    #[test]
    fn exclusive_lock_blocks_shared_lock_until_dropped() {
        let lock_dir = TestDir::new("excl-blocks-shared");
        let repo = lock_dir.0.join("repo");

        let excl = acquire_repo_lock_in(&lock_dir.0, "mutate", &repo, None).expect("excl lock");
        let (tx, rx) = mpsc::channel();
        let dir_clone = lock_dir.0.clone();
        let repo_clone = repo.clone();
        let shared_waiter = std::thread::spawn(move || {
            let _shared = acquire_repo_shared_lock_in(&dir_clone, "mutate", &repo_clone)
                .expect("shared lock");
            tx.send("shared").expect("send shared");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "shared lock acquired while exclusive lock held"
        );
        drop(excl);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("shared done"),
            "shared"
        );
        shared_waiter.join().expect("shared waiter join");
    }

    #[test]
    fn global_scan_slot_zero_returns_none_and_nonzero_throttles() {
        let lock_dir = TestDir::new("slots");
        assert!(
            acquire_global_scan_slot_in(&lock_dir.0, 0)
                .expect("zero slots")
                .is_none()
        );

        let slot1 = acquire_global_scan_slot_in(&lock_dir.0, 2)
            .expect("slot 1")
            .expect("some slot 1");
        let slot2 = acquire_global_scan_slot_in(&lock_dir.0, 2)
            .expect("slot 2")
            .expect("some slot 2");
        assert!(lock_dir.0.join("global-scan-slot-0.lock").is_file());
        assert!(lock_dir.0.join("global-scan-slot-1.lock").is_file());

        let (tx, rx) = mpsc::channel();
        let dir_clone = lock_dir.0.clone();
        let waiter = std::thread::spawn(move || {
            let _slot3 = acquire_global_scan_slot_in(&dir_clone, 2)
                .expect("slot 3")
                .expect("some slot 3");
            tx.send("acquired").expect("send slot 3");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "third scan slot acquired while both slots were occupied"
        );
        drop(slot1);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("slot 3 done"),
            "acquired"
        );
        drop(slot2);
        waiter.join().expect("slot waiter join");
    }

    #[test]
    fn global_scan_slot_caps_concurrent_workers() {
        let lock_dir = TestDir::new("slots-cap");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let dir = lock_dir.0.clone();
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    let _guard = acquire_global_scan_slot_in(&dir, 2)
                        .expect("slot ok")
                        .expect("some slot");
                    let cur = active.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(cur, Ordering::AcqRel);
                    std::thread::sleep(Duration::from_millis(30));
                    active.fetch_sub(1, Ordering::AcqRel);
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("worker join");
        }
        assert!(peak.load(Ordering::Acquire) <= 2);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }
}
