use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ub_runtime::{
    CacheDecoder, CacheEncoder, FileStamp, load_cache_file, repo_cache_path, save_cache_file,
};

use crate::model::Function;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_CACHED_FILES: usize = 25_000;
const TOOL_NAME: &str = "crapper";

/// Per-file AST analysis record cached between runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub stamp: FileStamp,
    pub language: String,
    pub relative: String,
    pub functions: Vec<Function>,
}

/// Returns the per-repository cache file path for `project_root`.
pub fn cache_path(project_root: &Path) -> PathBuf {
    repo_cache_path(TOOL_NAME, SCHEMA_VERSION, project_root)
}

fn encode_function(enc: &mut CacheEncoder, func: &Function) {
    enc.write_str(&func.name);
    enc.write_str(&func.namespace);
    enc.write_usize(func.complexity);
    enc.write_usize(func.start_line);
    enc.write_usize(func.end_line);
    enc.write_str(&func.path);
    enc.write_str(&func.language);
    enc.write_opt_str(func.jacoco_class.as_deref());
    enc.write_opt_usize(func.start_byte);
    enc.write_opt_usize(func.end_byte);
}

fn decode_function(dec: &mut CacheDecoder<'_>) -> Option<Function> {
    Some(Function {
        name: dec.read_str()?.to_string(),
        namespace: dec.read_str()?.to_string(),
        complexity: dec.read_usize()?,
        start_line: dec.read_usize()?,
        end_line: dec.read_usize()?,
        path: dec.read_str()?.to_string(),
        language: dec.read_str()?.to_string(),
        jacoco_class: dec.read_opt_str()?.map(str::to_string),
        start_byte: dec.read_opt_usize()?,
        end_byte: dec.read_opt_usize()?,
    })
}

/// Encodes per-file AST analysis records into a deterministic `UBC1` binary blob.
pub fn encode_cache(records: &HashMap<String, CachedFile>) -> Vec<u8> {
    let mut ordered: Vec<(&String, &CachedFile)> = records.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    ordered.truncate(MAX_CACHED_FILES);

    let mut enc = CacheEncoder::new();
    enc.write_usize(ordered.len());
    for (key, entry) in ordered {
        enc.write_str(key);
        enc.write_stamp(entry.stamp);
        enc.write_str(&entry.language);
        enc.write_str(&entry.relative);
        enc.write_usize(entry.functions.len());
        for func in &entry.functions {
            encode_function(&mut enc, func);
        }
    }
    enc.finish(SCHEMA_VERSION)
}

/// Decodes a `UBC1` binary blob into a map keyed by canonical POSIX path.
///
/// Returns `None` if the data is malformed, uses a mismatched schema version,
/// exceeds `MAX_CACHED_FILES`, or has trailing unconsumed bytes.
pub fn decode_cache(bytes: &[u8]) -> Option<HashMap<String, CachedFile>> {
    let mut dec = CacheDecoder::new(bytes, SCHEMA_VERSION)?;
    let file_count = dec.read_usize()?;
    if file_count > MAX_CACHED_FILES {
        return None;
    }

    let mut records = HashMap::with_capacity(file_count);
    for _ in 0..file_count {
        let key = dec.read_str()?.to_string();
        let stamp = dec.read_stamp()?;
        let language = dec.read_str()?.to_string();
        let relative = dec.read_str()?.to_string();
        let fn_count = dec.read_usize()?;
        let mut functions = Vec::with_capacity(fn_count.min(256));
        for _ in 0..fn_count {
            functions.push(decode_function(&mut dec)?);
        }
        records.insert(
            key,
            CachedFile {
                stamp,
                language,
                relative,
                functions,
            },
        );
    }

    if !dec.is_eof() {
        return None;
    }
    Some(records)
}

/// Loads cached AST records from `path`, returning an empty map if missing or corrupt.
pub fn load_cache(path: &Path) -> HashMap<String, CachedFile> {
    load_cache_file(path)
        .and_then(|bytes| decode_cache(&bytes))
        .unwrap_or_default()
}

/// Persists cached AST records atomically to `path` (best-effort).
pub fn save_cache(path: &Path, records: &HashMap<String, CachedFile>) {
    let bytes = encode_cache(records);
    save_cache_file(path, &bytes);
}

/// Merges `current_updates` into `existing`, keeping current-run entries first up to `MAX_CACHED_FILES`.
pub fn merge_cache(
    existing: HashMap<String, CachedFile>,
    current_updates: Vec<(String, CachedFile)>,
    current_hit_keys: &[String],
) -> HashMap<String, CachedFile> {
    merge_cache_with_limit(
        existing,
        current_updates,
        current_hit_keys,
        MAX_CACHED_FILES,
    )
}

pub(crate) fn merge_cache_with_limit(
    mut existing: HashMap<String, CachedFile>,
    current_updates: Vec<(String, CachedFile)>,
    current_hit_keys: &[String],
    limit: usize,
) -> HashMap<String, CachedFile> {
    let mut merged = HashMap::with_capacity((existing.len() + current_updates.len()).min(limit));

    for (key, record) in current_updates {
        if merged.len() >= limit && !merged.contains_key(&key) {
            break;
        }
        existing.remove(&key);
        merged.insert(key, record);
    }

    for key in current_hit_keys {
        if merged.len() >= limit {
            break;
        }
        if let Some(record) = existing.remove(key) {
            merged.insert(key.clone(), record);
        }
    }

    if merged.len() < limit && !existing.is_empty() {
        let mut remainder: Vec<(String, CachedFile)> = existing.into_iter().collect();
        remainder.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, record) in remainder {
            if merged.len() >= limit {
                break;
            }
            merged.insert(key, record);
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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
                "crapper-cache-{label}-{}-{nanos}-{id}",
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

    fn sample_stamp(seed: u64) -> FileStamp {
        FileStamp {
            mtime_ns: 1_700_000_000_000_000_000 + u128::from(seed),
            size: 100 + seed,
            extra_mtime_ns: 2_000_000 + u128::from(seed),
            extra_size: 42 + seed,
        }
    }

    fn sample_record(path: &str, language: &str, seed: u64) -> CachedFile {
        CachedFile {
            stamp: sample_stamp(seed),
            language: language.to_string(),
            relative: path.to_string(),
            functions: vec![
                Function {
                    name: "alpha".to_string(),
                    namespace: "pkg.Demo".to_string(),
                    complexity: 4,
                    start_line: 10,
                    end_line: 25,
                    path: path.to_string(),
                    language: language.to_string(),
                    jacoco_class: Some("pkg/Demo".to_string()),
                    start_byte: Some(120),
                    end_byte: Some(360),
                },
                Function {
                    name: "beta".to_string(),
                    namespace: "pkg.Demo".to_string(),
                    complexity: 1,
                    start_line: 27,
                    end_line: 30,
                    path: path.to_string(),
                    language: language.to_string(),
                    jacoco_class: None,
                    start_byte: None,
                    end_byte: None,
                },
            ],
        }
    }

    #[test]
    fn round_trips_all_function_fields_and_file_records() {
        let mut records = HashMap::new();
        records.insert(
            "/repo/src/Demo.java".to_string(),
            sample_record("src/Demo.java", "java", 1),
        );
        records.insert(
            "/repo/src/empty.py".to_string(),
            CachedFile {
                stamp: sample_stamp(2),
                language: "python".to_string(),
                relative: "src/empty.py".to_string(),
                functions: Vec::new(),
            },
        );

        let bytes = encode_cache(&records);
        let decoded = decode_cache(&bytes).expect("valid encoded cache");
        assert_eq!(decoded, records);
    }

    #[test]
    fn decode_rejects_corrupt_truncated_trailing_and_oversized_blobs() {
        let mut records = HashMap::new();
        records.insert("/repo/a.py".to_string(), sample_record("a.py", "python", 1));
        let valid = encode_cache(&records);

        assert!(decode_cache(&[]).is_none());
        assert!(decode_cache(b"not-a-cache").is_none());
        assert!(decode_cache(&valid[..valid.len() - 1]).is_none());

        let mut with_trailing = valid.clone();
        with_trailing.push(0xAA);
        assert!(decode_cache(&with_trailing).is_none());

        let mut wrong_version = valid;
        wrong_version[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert!(decode_cache(&wrong_version).is_none());

        let mut huge_file_count = CacheEncoder::new();
        huge_file_count.write_usize(MAX_CACHED_FILES + 1);
        assert!(decode_cache(&huge_file_count.finish(SCHEMA_VERSION)).is_none());

        let mut huge_fn_count = CacheEncoder::new();
        huge_fn_count.write_usize(1);
        huge_fn_count.write_str("/repo/a.py");
        huge_fn_count.write_stamp(sample_stamp(1));
        huge_fn_count.write_str("python");
        huge_fn_count.write_str("a.py");
        huge_fn_count.write_usize(usize::MAX);
        assert!(decode_cache(&huge_fn_count.finish(SCHEMA_VERSION)).is_none());
    }

    #[test]
    fn load_cache_ignores_missing_and_corrupt_files() {
        let dir = TestDir::new("corrupt");
        let missing = dir.0.join("missing.cache");
        assert!(load_cache(&missing).is_empty());

        let corrupt = dir.0.join("corrupt.cache");
        fs::write(&corrupt, b"UBC1\x01\x00\x00\x00garbage").expect("write corrupt cache");
        assert!(load_cache(&corrupt).is_empty());

        let mut records = HashMap::new();
        records.insert("/repo/a.py".to_string(), sample_record("a.py", "python", 3));
        save_cache(&corrupt, &records);
        assert_eq!(load_cache(&corrupt), records);
    }

    #[test]
    fn bounded_merge_keeps_current_run_entries_first() {
        let mut existing = HashMap::new();
        existing.insert(
            "/repo/old_a.py".to_string(),
            sample_record("old_a.py", "python", 1),
        );
        existing.insert(
            "/repo/old_b.py".to_string(),
            sample_record("old_b.py", "python", 2),
        );
        existing.insert(
            "/repo/hit.py".to_string(),
            sample_record("hit.py", "python", 3),
        );
        existing.insert(
            "/repo/stale.py".to_string(),
            sample_record("stale.py", "python", 4),
        );

        let updates = vec![
            (
                "/repo/new.py".to_string(),
                sample_record("new.py", "python", 10),
            ),
            (
                "/repo/stale.py".to_string(),
                sample_record("stale.py", "python", 20),
            ),
        ];
        let hit_keys = vec!["/repo/hit.py".to_string()];

        let merged = merge_cache_with_limit(existing, updates, &hit_keys, 4);
        assert_eq!(merged.len(), 4);
        assert!(merged.contains_key("/repo/new.py"));
        assert_eq!(merged["/repo/stale.py"].stamp, sample_stamp(20));
        assert!(merged.contains_key("/repo/hit.py"));
        assert!(merged.contains_key("/repo/old_a.py"));

        let tighter = merge_cache_with_limit(
            merged,
            vec![(
                "/repo/latest.py".to_string(),
                sample_record("latest.py", "python", 30),
            )],
            &["/repo/hit.py".to_string()],
            2,
        );
        assert_eq!(tighter.len(), 2);
        assert!(tighter.contains_key("/repo/latest.py"));
        assert!(tighter.contains_key("/repo/hit.py"));
    }
}
