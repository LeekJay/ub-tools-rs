use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use uuid::Uuid;

const MAX_RENAME_ATTEMPTS: usize = 25;
const RENAME_BACKOFF_STEP_MS: u64 = 5;
const RENAME_BACKOFF_MAX_MS: u64 = 50;

fn unique_sibling_temp_path(path: &Path) -> io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic_write target path has no file name",
        )
    })?;

    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ));

    Ok(match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(&temp_name),
        _ => PathBuf::from(&temp_name),
    })
}

fn write_and_flush_temp(temp_path: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    file.write_all(content)?;
    file.flush()
}

fn is_retryable_rename_error(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AlreadyExists
    ) {
        return true;
    }
    #[cfg(windows)]
    if matches!(err.raw_os_error(), Some(5 | 32)) {
        return true;
    }
    false
}

fn rename_with_backoff(temp_path: &Path, target: &Path) -> io::Result<()> {
    for attempt in 0..MAX_RENAME_ATTEMPTS {
        match fs::rename(temp_path, target) {
            Ok(()) => return Ok(()),
            Err(err) if attempt + 1 < MAX_RENAME_ATTEMPTS && is_retryable_rename_error(&err) => {
                let delay_ms =
                    ((attempt as u64 + 1) * RENAME_BACKOFF_STEP_MS).min(RENAME_BACKOFF_MAX_MS);
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("loop returns within MAX_RENAME_ATTEMPTS")
}

/// Atomically writes `content` to `path` via a unique sibling temp file and retry-backed rename.
pub fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let temp_path = unique_sibling_temp_path(path)?;
    if let Err(err) = write_and_flush_temp(&temp_path, content) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    if let Err(err) = rename_with_backoff(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ub-runtime-atomic-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
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

    #[test]
    fn creates_nested_parents_and_overwrites_content() {
        let dir = TestDir::new("nested");
        let target = dir.0.join("nested").join("deep").join("metrics.edn");

        atomic_write(&target, b"first payload").expect("first write");
        assert_eq!(fs::read(&target).expect("read first"), b"first payload");

        atomic_write(&target, b"second").expect("overwrite write");
        assert_eq!(fs::read(&target).expect("read second"), b"second");

        let leftover_tmps: Vec<_> = fs::read_dir(target.parent().expect("parent"))
            .expect("read parent")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "leftover tmp files: {leftover_tmps:?}"
        );
    }

    #[test]
    fn rejects_path_without_file_name() {
        let err = atomic_write(Path::new(""), b"abc").expect_err("empty path fails");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn supports_bare_filename_and_binary_or_empty_content() {
        let bare = unique_sibling_temp_path(Path::new("plain.edn")).expect("bare temp path");
        assert_eq!(bare.parent(), Some(Path::new("")));

        let dir = TestDir::new("binary");
        let target = dir.0.join("blob.bin");
        atomic_write(&target, b"\x00\xff\x01\xfe").expect("binary write");
        assert_eq!(fs::read(&target).expect("read binary"), b"\x00\xff\x01\xfe");

        atomic_write(&target, b"").expect("empty write");
        assert_eq!(fs::read(&target).expect("read empty"), b"");
    }

    #[test]
    fn cleans_up_temp_file_when_rename_fails_on_directory_target() {
        let dir = TestDir::new("fail-cleanup");
        let existing_dir = dir.0.join("occupied");
        fs::create_dir_all(existing_dir.join("inner")).expect("create occupied dir");

        let err = atomic_write(&existing_dir, b"cannot overwrite dir")
            .expect_err("renaming file over directory must fail");
        assert_ne!(err.kind(), io::ErrorKind::InvalidInput);

        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("read parent")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp file leaked after rename error: {leftovers:?}"
        );
    }

    #[test]
    fn temp_file_format_matches_contract() {
        let path = Path::new("some").join("dir").join("crap.edn");
        let tmp = unique_sibling_temp_path(&path).expect("temp path");
        assert_eq!(tmp.parent(), path.parent());
        let name = tmp
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name");
        let prefix = format!(".crap.edn.{}.", std::process::id());
        assert!(name.starts_with(&prefix), "unexpected tmp name: {name}");
        assert!(name.ends_with(".tmp"), "unexpected tmp suffix: {name}");
        let uuid_part = &name[prefix.len()..name.len() - 4];
        assert_eq!(uuid_part.len(), 32);
        assert!(uuid_part.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn concurrent_writer_stress_leaves_valid_payload_and_no_temp_files() {
        let dir = TestDir::new("stress");
        let target = Arc::new(dir.0.join("shared.edn"));
        let done = Arc::new(AtomicBool::new(false));
        let writers = 10usize;
        let iters = 25usize;

        let reader_target = Arc::clone(&target);
        let reader_done = Arc::clone(&done);
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                match fs::read(&*reader_target) {
                    Ok(bytes) => {
                        let text = String::from_utf8(bytes).expect("valid utf-8 payload");
                        let lines: Vec<&str> = text.lines().collect();
                        assert_eq!(lines.len(), 64, "torn read detected: {} lines", lines.len());
                        assert!(
                            lines.iter().all(|line| *line == lines[0]),
                            "mixed writer lines detected"
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    // On Windows, concurrent reader `OpenFile` can hit transient sharing/access errors during replace.
                    Err(err)
                        if err.kind() == io::ErrorKind::PermissionDenied
                            || matches!(err.raw_os_error(), Some(5 | 32)) => {}
                    Err(err) => panic!("unexpected read error: {err}"),
                }
            }
        });

        let handles: Vec<_> = (0..writers)
            .map(|tid| {
                let target = Arc::clone(&target);
                std::thread::spawn(move || {
                    for iter in 0..iters {
                        let line = format!("writer-{tid:02}-iter-{iter:03}\n");
                        let payload = line.repeat(64);
                        atomic_write(&target, payload.as_bytes())
                            .unwrap_or_else(|err| panic!("writer {tid} iter {iter} failed: {err}"));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("writer thread panicked");
        }
        done.store(true, Ordering::Release);
        reader.join().expect("reader thread panicked");

        let final_text = fs::read_to_string(&*target).expect("final read");
        let final_lines: Vec<&str> = final_text.lines().collect();
        assert_eq!(final_lines.len(), 64);
        assert!(final_lines.iter().all(|line| *line == final_lines[0]));

        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("read stress dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "shared.edn")
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {leftovers:?}");
    }
}
