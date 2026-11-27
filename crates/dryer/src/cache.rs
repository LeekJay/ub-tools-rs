//! Incremental per-repository AST and hashed-fingerprint cache (`UBC1` binary format).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use ub_runtime::{
    CacheDecoder, CacheEncoder, FileStamp, load_cache_file, repo_cache_path, save_cache_file,
};

use super::model::Entry;

pub const SCHEMA_VERSION: u32 = 2;
pub const MAX_CACHED_FILES: usize = 25_000;
const TOOL_NAME: &str = "dryer";

/// Normalized form entry storing sorted unique 128-bit truncated SHA-256 fingerprint hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashedEntry {
    pub language: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub nodes: usize,
    pub fingerprints: Vec<u128>,
}

/// Compute a truncated 128-bit SHA-256 hash of a normalized subtree fingerprint.
pub fn hash_fingerprint(fp: &str) -> u128 {
    let digest = Sha256::digest(fp.as_bytes());
    let bytes: [u8; 16] = digest[..16].try_into().expect("sha256 >= 16 bytes");
    u128::from_le_bytes(bytes)
}

impl HashedEntry {
    /// Convert a raw `Entry` into a compact `HashedEntry` with sorted unique fingerprint hashes.
    pub fn from_entry(entry: &Entry) -> Self {
        let mut fingerprints: Vec<u128> = entry
            .fingerprints
            .iter()
            .map(|fp| hash_fingerprint(fp))
            .collect();
        fingerprints.sort_unstable();
        fingerprints.dedup();
        Self {
            language: entry.language.clone(),
            file: entry.file.clone(),
            start_line: entry.start_line,
            end_line: entry.end_line,
            nodes: entry.nodes,
            fingerprints,
        }
    }
}

/// Cached parse output for one source file prior to line/node threshold filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub stamp: FileStamp,
    pub language: String,
    pub display_file: String,
    pub raw_entries: Vec<HashedEntry>,
    pub warning: Option<String>,
}

/// Decoded repository cache preserving active-first priority order for bounded merging.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DryerCache {
    files: HashMap<String, CachedFile>,
    order: Vec<String>,
}

impl DryerCache {
    pub fn repo_path(root: &Path) -> PathBuf {
        repo_cache_path(TOOL_NAME, SCHEMA_VERSION, root)
    }

    pub fn load(root: &Path) -> Self {
        Self::load_from(&Self::repo_path(root))
    }

    pub fn load_from(path: &Path) -> Self {
        let Some(bytes) = load_cache_file(path) else {
            return Self::default();
        };
        decode_cache(&bytes).unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<&CachedFile> {
        self.files.get(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<CachedFile> {
        self.files.remove(key)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn ordered_keys(&self) -> &[String] {
        &self.order
    }

    pub fn merge_and_save(self, root: &Path, active: Vec<(String, CachedFile)>) {
        self.merge_and_save_to(&Self::repo_path(root), active);
    }

    pub fn merge_and_save_to(self, path: &Path, active: Vec<(String, CachedFile)>) {
        let merged = merge_entries(active, self, MAX_CACHED_FILES);
        let bytes = encode_entries(&merged);
        save_cache_file(path, &bytes);
    }
}

/// Merge `active` entries first followed by untouched `prior` entries, capped at `max_files`.
pub fn merge_entries(
    active: Vec<(String, CachedFile)>,
    mut prior: DryerCache,
    max_files: usize,
) -> Vec<(String, CachedFile)> {
    let mut seen = HashSet::with_capacity(active.len().min(max_files));
    let mut merged = Vec::with_capacity(max_files.min(active.len() + prior.order.len()));

    for (key, item) in active {
        if merged.len() >= max_files {
            break;
        }
        if seen.insert(key.clone()) {
            merged.push((key, item));
        }
    }

    for key in prior.order {
        if merged.len() >= max_files {
            break;
        }
        if seen.insert(key.clone())
            && let Some(item) = prior.files.remove(&key)
        {
            merged.push((key, item));
        }
    }

    merged
}

fn write_entry(enc: &mut CacheEncoder, entry: &HashedEntry) {
    enc.write_str(&entry.language);
    enc.write_str(&entry.file);
    enc.write_usize(entry.start_line);
    enc.write_usize(entry.end_line);
    enc.write_usize(entry.nodes);
    enc.write_usize(entry.fingerprints.len());

    for &fp in &entry.fingerprints {
        enc.write_u128(fp);
    }
}

fn read_entry(dec: &mut CacheDecoder<'_>) -> Option<HashedEntry> {
    let language = dec.read_str()?.to_string();
    let file = dec.read_str()?.to_string();
    let start_line = dec.read_usize()?;
    let end_line = dec.read_usize()?;
    let nodes = dec.read_usize()?;
    let fp_count = dec.read_usize()?;
    let mut fingerprints = Vec::with_capacity(fp_count.min(4096));
    for _ in 0..fp_count {
        fingerprints.push(dec.read_u128()?);
    }
    Some(HashedEntry {
        language,
        file,
        start_line,
        end_line,
        nodes,
        fingerprints,
    })
}

/// Encode `entries` using `CacheEncoder` for metadata while storing 128-bit fingerprint hashes inline.
pub fn encode_entries(entries: &[(String, CachedFile)]) -> Vec<u8> {
    let capped = &entries[..entries.len().min(MAX_CACHED_FILES)];
    let mut enc = CacheEncoder::new();
    enc.write_usize(capped.len());
    for (key, cached) in capped {
        enc.write_str(key);
        enc.write_stamp(cached.stamp);
        enc.write_str(&cached.language);
        enc.write_str(&cached.display_file);
        enc.write_opt_str(cached.warning.as_deref());
        enc.write_usize(cached.raw_entries.len());
        for entry in &cached.raw_entries {
            write_entry(&mut enc, entry);
        }
    }
    enc.finish(SCHEMA_VERSION)
}

/// Decode a `UBC1` cache payload, validating schema version, bounds, and trailing EOF.
pub fn decode_cache(bytes: &[u8]) -> Option<DryerCache> {
    let mut dec = CacheDecoder::new(bytes, SCHEMA_VERSION)?;
    let file_count = dec.read_usize()?;
    if file_count > MAX_CACHED_FILES {
        return None;
    }

    let mut files = HashMap::with_capacity(file_count.min(4096));
    let mut order = Vec::with_capacity(file_count.min(4096));
    for _ in 0..file_count {
        let key = dec.read_str()?.to_string();
        let stamp = dec.read_stamp()?;
        let language = dec.read_str()?.to_string();
        let display_file = dec.read_str()?.to_string();
        let warning = dec.read_opt_str()?.map(str::to_string);
        let entry_count = dec.read_usize()?;
        let mut raw_entries = Vec::with_capacity(entry_count.min(1024));
        for _ in 0..entry_count {
            raw_entries.push(read_entry(&mut dec)?);
        }
        if files.contains_key(&key) {
            return None;
        }
        order.push(key.clone());
        files.insert(
            key,
            CachedFile {
                stamp,
                language,
                display_file,
                raw_entries,
                warning,
            },
        );
    }

    if !dec.is_eof() {
        return None;
    }

    Some(DryerCache { files, order })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cached_file(display: &str, fp_shared: &str, warning: Option<&str>) -> CachedFile {
        let raw = Entry {
            language: "clojure".to_string(),
            file: display.to_string(),
            start_line: 3,
            end_line: 7,
            nodes: 12,
            fingerprints: HashSet::from([fp_shared.to_string(), "[:list :symbol]".to_string()]),
        };
        CachedFile {
            stamp: FileStamp {
                mtime_ns: 1_700_000_000_123,
                size: 42,
                extra_mtime_ns: 0,
                extra_size: 0,
            },
            language: "clojure".to_string(),
            display_file: display.to_string(),
            raw_entries: vec![HashedEntry::from_entry(&raw)],
            warning: warning.map(str::to_string),
        }
    }

    #[test]
    fn encode_decode_round_trips_and_stores_hashed_fingerprints_compactly() {
        let f1 = sample_cached_file("src/a.clj", "[:shared]", Some("unmatched quote"));
        let f2 = sample_cached_file("src/b.clj", "[:shared]", None);
        let bytes = encode_entries(&[
            ("/repo/src/a.clj".to_string(), f1.clone()),
            ("/repo/src/b.clj".to_string(), f2.clone()),
        ]);

        // String table count is at header bytes 8..12:
        // keys (2) + "clojure" + displays (2) + "unmatched quote" = 6 unique strings
        // (fingerprints are stored as packed 128-bit hashes rather than AST strings).
        let string_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(string_count, 6);

        let decoded = decode_cache(&bytes).expect("valid cache");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("/repo/src/a.clj"), Some(&f1));
        assert_eq!(decoded.get("/repo/src/b.clj"), Some(&f2));
        assert_eq!(
            decoded.ordered_keys(),
            &["/repo/src/a.clj".to_string(), "/repo/src/b.clj".to_string()]
        );
    }

    #[test]
    fn decode_rejects_corrupt_truncated_trailing_and_wrong_version_blobs() {
        let f1 = sample_cached_file("src/a.clj", "[:fp]", None);
        let valid = encode_entries(&[("/repo/src/a.clj".to_string(), f1)]);

        assert!(decode_cache(b"not a cache").is_none());
        assert!(decode_cache(&valid[..valid.len() - 1]).is_none());

        let mut trailing = valid.clone();
        trailing.push(0x99);
        assert!(decode_cache(&trailing).is_none());

        let mut wrong_version = valid.clone();
        wrong_version[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert!(decode_cache(&wrong_version).is_none());

        let mut absurd_count = CacheEncoder::new();
        absurd_count.write_usize(MAX_CACHED_FILES + 1);
        assert!(decode_cache(&absurd_count.finish(SCHEMA_VERSION)).is_none());
    }

    #[test]
    fn merge_keeps_active_entries_first_and_caps_at_limit() {
        let prior_items: Vec<(String, CachedFile)> = (0..4)
            .map(|i| {
                let key = format!("/repo/old_{i}.clj");
                let file = sample_cached_file(&format!("old_{i}.clj"), "[:old]", None);
                (key, file)
            })
            .collect();
        let prior = decode_cache(&encode_entries(&prior_items)).unwrap();

        let active = vec![
            (
                "/repo/active.clj".to_string(),
                sample_cached_file("active.clj", "[:new]", None),
            ),
            (
                "/repo/old_1.clj".to_string(),
                sample_cached_file("old_1.clj", "[:updated]", None),
            ),
        ];

        let merged = merge_entries(active, prior, 3);
        let keys: Vec<&str> = merged.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["/repo/active.clj", "/repo/old_1.clj", "/repo/old_0.clj"]
        );
        assert!(
            merged[1].1.raw_entries[0]
                .fingerprints
                .contains(&hash_fingerprint("[:updated]"))
        );
    }

    #[test]
    fn load_from_ignores_missing_or_corrupt_files() {
        let dir = std::env::temp_dir().join(format!(
            "dryer-cache-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.cache");
        assert!(DryerCache::load_from(&missing).is_empty());

        let corrupt = dir.join("corrupt.cache");
        std::fs::write(&corrupt, b"corrupt-bytes").unwrap();
        assert!(DryerCache::load_from(&corrupt).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
