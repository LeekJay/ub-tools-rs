//! Persistent per-repository mutation-site cache backed by `ub_runtime` `UBC1` blobs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ub_runtime::{
    CacheDecoder, CacheEncoder, FileStamp, ManifestStampCache, load_cache_file,
    repo_cache_path as runtime_repo_cache_path, save_cache_file,
};

use crate::model::Site;
use crate::path_util::{resolve_path, to_posix};

/// Binary cache schema version for `mutator` site inventories.
pub const SCHEMA_VERSION: u32 = 2;

/// Maximum number of source files retained in one repository cache file.
pub const MAX_CACHED_FILES: usize = 25_000;

const TOOL_NAME: &str = "mutator";
const MAX_PREALLOC: usize = 1024;

/// Cached site discovery output and invalidation stamp for one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSites {
    pub stamp: FileStamp,
    pub file_key: String,
    pub language: String,
    pub sites: Vec<Site>,
}

/// In-memory view of the per-repository mutation-site cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteCache {
    entries: HashMap<String, CachedSites>,
}

/// Canonical POSIX path key used to index `SiteCache` entries.
pub fn canonical_path_key(path: &Path) -> String {
    to_posix(&resolve_path(path))
}

/// Cache file path for `root` under the configured `ub_runtime` cache directory.
pub fn repo_cache_path(root: &Path) -> PathBuf {
    runtime_repo_cache_path(TOOL_NAME, SCHEMA_VERSION, root)
}

/// Compute `(file, nearest_manifest)` modification stamp for `path`.
pub fn file_stamp(
    path: &Path,
    language: &str,
    manifests: &mut ManifestStampCache,
) -> Option<FileStamp> {
    let (extra_mtime_ns, extra_size) = manifests.stamp_for(path, language);
    FileStamp::from_paths(path, extra_mtime_ns, extra_size)
}

fn encode_site(enc: &mut CacheEncoder, site: &Site) {
    enc.write_str(&site.file);
    enc.write_str(&site.namespace);
    enc.write_str(&site.form_id);
    enc.write_str(&site.name);
    enc.write_usize(site.line);
    enc.write_usize(site.start);
    enc.write_usize(site.end);
    enc.write_str(&site.original);
    enc.write_str(&site.mutant);
    enc.write_str(&site.category);
    enc.write_raw_str(&site.mutation_id);
    enc.write_usize(site.anchor_line);
}

fn decode_site(dec: &mut CacheDecoder<'_>) -> Option<Site> {
    Some(Site {
        file: dec.read_str()?.to_string(),
        namespace: dec.read_str()?.to_string(),
        form_id: dec.read_str()?.to_string(),
        name: dec.read_str()?.to_string(),
        line: dec.read_usize()?,
        start: dec.read_usize()?,
        end: dec.read_usize()?,
        original: dec.read_str()?.to_string(),
        mutant: dec.read_str()?.to_string(),
        category: dec.read_str()?.to_string(),
        mutation_id: dec.read_raw_str()?.to_string(),
        anchor_line: dec.read_usize()?,
    })
}

impl SiteCache {
    /// Create an empty site cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached source files currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no cached files are stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Load the repository cache for `root`, returning empty on missing or corrupt blobs.
    pub fn load(root: &Path) -> Self {
        Self::load_from(&repo_cache_path(root))
    }

    /// Load a cache file from `path`, returning empty on missing or corrupt blobs.
    pub fn load_from(path: &Path) -> Self {
        load_cache_file(path)
            .and_then(|bytes| Self::decode(&bytes))
            .unwrap_or_default()
    }

    /// Look up cached sites if the stamp, relative `file_key`, and `language` all match.
    pub fn get(
        &self,
        canonical_key: &str,
        stamp: FileStamp,
        file_key: &str,
        language: &str,
    ) -> Option<&[Site]> {
        let entry = self.entries.get(canonical_key)?;
        if entry.stamp == stamp && entry.file_key == file_key && entry.language == language {
            Some(&entry.sites)
        } else {
            None
        }
    }

    /// Insert or replace an entry in memory (primarily for tests).
    pub fn insert(&mut self, canonical_key: String, entry: CachedSites) {
        self.entries.insert(canonical_key, entry);
    }

    /// Decode a `UBC1` binary blob, validating schema version, bounds, and `is_eof()`.
    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut dec = CacheDecoder::new(data, SCHEMA_VERSION)?;
        let count = dec.read_usize()?;
        if count > MAX_CACHED_FILES {
            return None;
        }
        let mut entries = HashMap::with_capacity(count.min(MAX_PREALLOC));
        for _ in 0..count {
            let canonical_key = dec.read_str()?.to_string();
            let stamp = dec.read_stamp()?;
            let file_key = dec.read_str()?.to_string();
            let language = dec.read_str()?.to_string();
            let site_count = dec.read_usize()?;
            let mut sites = Vec::with_capacity(site_count.min(MAX_PREALLOC));
            for _ in 0..site_count {
                sites.push(decode_site(&mut dec)?);
            }
            entries.insert(
                canonical_key,
                CachedSites {
                    stamp,
                    file_key,
                    language,
                    sites,
                },
            );
        }
        if !dec.is_eof() {
            return None;
        }
        Some(Self { entries })
    }

    /// Encode all cached entries deterministically with interned strings.
    pub fn encode(&self) -> Vec<u8> {
        let mut keys: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        keys.sort_unstable();

        let mut enc = CacheEncoder::new();
        enc.write_usize(keys.len());
        for key in keys {
            let entry = &self.entries[key];
            enc.write_str(key);
            enc.write_stamp(entry.stamp);
            enc.write_str(&entry.file_key);
            enc.write_str(&entry.language);
            enc.write_usize(entry.sites.len());
            for site in &entry.sites {
                encode_site(&mut enc, site);
            }
        }
        enc.finish(SCHEMA_VERSION)
    }

    /// Merge `updates` and cap total entries at `max_files`, keeping `active_keys` first.
    pub fn merge_and_cap<I>(&mut self, updates: I, active_keys: &HashSet<String>, max_files: usize)
    where
        I: IntoIterator<Item = (String, CachedSites)>,
    {
        for (key, entry) in updates {
            self.entries.insert(key, entry);
        }
        if self.entries.len() <= max_files {
            return;
        }
        let mut active: Vec<String> = self
            .entries
            .keys()
            .filter(|k| active_keys.contains(k.as_str()))
            .cloned()
            .collect();
        active.sort_unstable();
        active.truncate(max_files);

        let remaining = max_files.saturating_sub(active.len());
        let mut inactive: Vec<String> = if remaining > 0 {
            self.entries
                .keys()
                .filter(|k| !active_keys.contains(k.as_str()))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        inactive.sort_unstable();
        inactive.truncate(remaining);

        let keep: HashSet<String> = active.into_iter().chain(inactive).collect();
        self.entries.retain(|k, _| keep.contains(k));
    }

    /// Persist the cache atomically to `path`.
    pub fn save_to(&self, path: &Path) {
        let bytes = self.encode();
        save_cache_file(path, &bytes);
    }

    /// Merge `updates`, cap at `MAX_CACHED_FILES` keeping active run entries first, and save once.
    pub fn merge_and_save_to<I>(&mut self, path: &Path, updates: I, active_keys: &HashSet<String>)
    where
        I: IntoIterator<Item = (String, CachedSites)>,
    {
        self.merge_and_cap(updates, active_keys, MAX_CACHED_FILES);
        self.save_to(path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mutator_cache_test_{label}_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_site(file: &str, line: usize, anchor_line: usize) -> Site {
        Site {
            file: file.to_string(),
            namespace: "demo.core".to_string(),
            form_id: "defn/check".to_string(),
            name: "check".to_string(),
            line,
            start: 14,
            end: 16,
            original: ">=".to_string(),
            mutant: ">".to_string(),
            category: "comparison".to_string(),
            mutation_id: format!("[\"{file}\",\"demo.core\",\"defn/check\",14,16,\">=\",\">\"]"),
            anchor_line,
        }
    }

    #[test]
    fn encode_decode_round_trip_preserves_all_site_fields_and_interns_strings() {
        let stamp = FileStamp {
            mtime_ns: 987_654_321_123_456,
            size: 512,
            extra_mtime_ns: 42,
            extra_size: 128,
        };
        let site1 = sample_site("src/demo.rs", 10, 9);
        let mut site2 = sample_site("src/demo.rs", 14, 14);
        site2.start = 40;
        site2.end = 42;
        site2.mutation_id =
            "[\"src/demo.rs\",\"demo.core\",\"defn/check\",40,42,\">=\",\">\"]".to_string();

        let mut cache = SiteCache::new();
        cache.insert(
            "D:/repo/src/demo.rs".to_string(),
            CachedSites {
                stamp,
                file_key: "src/demo.rs".to_string(),
                language: "rust".to_string(),
                sites: vec![site1.clone(), site2.clone()],
            },
        );

        let bytes = cache.encode();
        // Repeated strings across site1/site2 (`src/demo.rs`, `demo.core`, `defn/check`, `check`,
        // `>=`, `>`, `comparison`) plus keys are interned; unique `mutation_id`s stay inline.
        let string_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(string_count, 9);

        let decoded = SiteCache::decode(&bytes).expect("decode round-trip");
        assert_eq!(decoded, cache);
        assert_eq!(
            decoded.get("D:/repo/src/demo.rs", stamp, "src/demo.rs", "rust"),
            Some([site1, site2].as_slice())
        );
    }

    #[test]
    fn decode_rejects_corrupt_truncated_trailing_or_oversized_blobs() {
        let mut cache = SiteCache::new();
        cache.insert(
            "/tmp/a.py".to_string(),
            CachedSites {
                stamp: FileStamp {
                    mtime_ns: 1,
                    size: 2,
                    extra_mtime_ns: 0,
                    extra_size: 0,
                },
                file_key: "a.py".to_string(),
                language: "python".to_string(),
                sites: vec![sample_site("a.py", 3, 3)],
            },
        );
        let valid = cache.encode();

        let mut trailing = valid.clone();
        trailing.push(0xAA);
        assert!(SiteCache::decode(&trailing).is_none());

        assert!(SiteCache::decode(&valid[..valid.len() - 1]).is_none());
        assert!(SiteCache::decode(b"not-a-cache").is_none());

        let mut bad_version = valid.clone();
        bad_version[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert!(SiteCache::decode(&bad_version).is_none());

        let mut enc = CacheEncoder::new();
        enc.write_usize(MAX_CACHED_FILES + 1);
        assert!(SiteCache::decode(&enc.finish(SCHEMA_VERSION)).is_none());

        let dir = temp_dir("corrupt_io");
        let cache_file = dir.join("site.cache");
        fs::write(&cache_file, &trailing).unwrap();
        assert!(SiteCache::load_from(&cache_file).is_empty());

        fs::create_dir_all(dir.join("dir_as_cache")).unwrap();
        assert!(SiteCache::load_from(&dir.join("dir_as_cache")).is_empty());
        assert!(SiteCache::load_from(&dir.join("missing.cache")).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_and_source_changes_invalidate_cached_stamps() {
        let dir = temp_dir("manifest_stamp");
        let manifests = [
            (
                "rust",
                "Cargo.toml",
                "src/lib.rs",
                "[package]\nname=\"x\"\n",
            ),
            ("go", "go.mod", "pkg/demo.go", "module demo\n"),
            (
                "swift",
                "Package.swift",
                "Sources/App/Main.swift",
                "// swift-tools-version:5.9\n",
            ),
        ];

        for (lang, manifest_name, rel_src, initial_manifest) in manifests {
            let sub_root = dir.join(lang);
            let manifest_path = sub_root.join(manifest_name);
            let src_path = sub_root.join(rel_src);
            fs::create_dir_all(src_path.parent().unwrap()).unwrap();
            fs::write(&manifest_path, initial_manifest).unwrap();
            fs::write(&src_path, "initial\n").unwrap();

            let mut manifest_cache = ManifestStampCache::new();
            let s0 = file_stamp(&src_path, lang, &mut manifest_cache).unwrap();

            let mut cache = SiteCache::new();
            let key = canonical_path_key(&src_path);
            cache.insert(
                key.clone(),
                CachedSites {
                    stamp: s0,
                    file_key: rel_src.to_string(),
                    language: lang.to_string(),
                    sites: vec![sample_site(rel_src, 1, 1)],
                },
            );
            assert!(cache.get(&key, s0, rel_src, lang).is_some());

            // Manifest edit invalidates stamp even when source file is untouched.
            fs::write(&manifest_path, format!("{initial_manifest}# updated\n")).unwrap();
            let mut manifest_cache2 = ManifestStampCache::new();
            let s1 = file_stamp(&src_path, lang, &mut manifest_cache2).unwrap();
            assert_ne!(s0, s1);
            assert!(cache.get(&key, s1, rel_src, lang).is_none());

            // Source edit invalidates stamp as well.
            fs::write(&src_path, "initial\nupdated\n").unwrap();
            let mut manifest_cache3 = ManifestStampCache::new();
            let s2 = file_stamp(&src_path, lang, &mut manifest_cache3).unwrap();
            assert_ne!(s1, s2);
            assert!(cache.get(&key, s2, rel_src, lang).is_none());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_and_cap_keeps_active_entries_first() {
        let stamp = FileStamp {
            mtime_ns: 1,
            size: 10,
            extra_mtime_ns: 0,
            extra_size: 0,
        };
        let make_entry = |name: &str| CachedSites {
            stamp,
            file_key: name.to_string(),
            language: "python".to_string(),
            sites: Vec::new(),
        };

        let mut cache = SiteCache::new();
        for i in 0..8 {
            cache.insert(
                format!("old/{i:02}.py"),
                make_entry(&format!("old/{i:02}.py")),
            );
        }

        let active = HashSet::from([
            "old/02.py".to_string(),
            "new/a.py".to_string(),
            "new/b.py".to_string(),
        ]);
        let updates = vec![
            ("new/a.py".to_string(), make_entry("new/a.py")),
            ("new/b.py".to_string(), make_entry("new/b.py")),
        ];
        cache.merge_and_cap(updates, &active, 5);
        assert_eq!(cache.len(), 5);
        for key in &active {
            assert!(
                cache.get(key, stamp, key, "python").is_some(),
                "expected active key {key} to be retained"
            );
        }

        // Also verify MAX_CACHED_FILES constant is enforced by merge_and_save_to.
        let dir = temp_dir("cap_max");
        let path = dir.join("capped.cache");
        let mut big = SiteCache::new();
        for i in 0..MAX_CACHED_FILES {
            let k = format!("old/{i:05}.py");
            big.insert(k.clone(), make_entry(&k));
        }
        let active_big = HashSet::from(["active/hot.py".to_string()]);
        big.merge_and_save_to(
            &path,
            vec![("active/hot.py".to_string(), make_entry("active/hot.py"))],
            &active_big,
        );
        let reloaded = SiteCache::load_from(&path);
        assert_eq!(reloaded.len(), MAX_CACHED_FILES);
        assert!(
            reloaded
                .get("active/hot.py", stamp, "active/hot.py", "python")
                .is_some()
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
