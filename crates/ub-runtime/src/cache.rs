use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::atomic::atomic_write;
use crate::lock::repo_hash;
use crate::runtime::parse_env_no_metrics;

const ENV_CACHE_DIR: &str = "UB_TOOLS_CACHE_DIR";
const ENV_NO_CACHE: &str = "UB_TOOLS_NO_CACHE";
const CACHE_DIR_NAME: &str = "ub-tools-cache";
const MAGIC: [u8; 4] = *b"UBC1";
const HEADER_LEN: usize = 16;

/// File content and companion manifest modification stamps used for cache invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime_ns: u128,
    pub size: u64,
    pub extra_mtime_ns: u128,
    pub extra_size: u64,
}

impl FileStamp {
    pub fn from_paths(file: &Path, extra_mtime_ns: u128, extra_size: u64) -> Option<Self> {
        let (mtime_ns, size) = file_mtime_size(file)?;
        Some(Self {
            mtime_ns,
            size,
            extra_mtime_ns,
            extra_size,
        })
    }
}

/// Returns the base cache directory (`UB_TOOLS_CACHE_DIR`, else OS user cache dir).
pub fn cache_dir() -> PathBuf {
    cache_dir_from(
        std::env::var(ENV_CACHE_DIR).ok().as_deref(),
        #[cfg(windows)]
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        #[cfg(not(windows))]
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        #[cfg(not(windows))]
        std::env::var("HOME").ok().as_deref(),
    )
}

fn cache_dir_from(
    override_dir: Option<&str>,
    #[cfg(windows)] local_app_data: Option<&str>,
    #[cfg(not(windows))] xdg_cache_home: Option<&str>,
    #[cfg(not(windows))] home: Option<&str>,
) -> PathBuf {
    if let Some(dir) = override_dir.map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        if let Some(base) = local_app_data.map(str::trim).filter(|s| !s.is_empty()) {
            return PathBuf::from(base).join(CACHE_DIR_NAME);
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(base) = xdg_cache_home.map(str::trim).filter(|s| !s.is_empty()) {
            return PathBuf::from(base).join(CACHE_DIR_NAME);
        }
        if let Some(base) = home.map(str::trim).filter(|s| !s.is_empty()) {
            return PathBuf::from(base).join(".cache").join(CACHE_DIR_NAME);
        }
    }
    std::env::temp_dir().join(CACHE_DIR_NAME)
}

/// Returns `true` when `UB_TOOLS_NO_CACHE` is `"1"`, `"true"`, or `"yes"`.
pub fn env_no_cache() -> bool {
    let raw = std::env::var(ENV_NO_CACHE).ok();
    parse_env_no_metrics(raw.as_deref())
}

/// Returns the per-repository cache file path for `tool` and `schema_version`.
pub fn repo_cache_path(tool: &str, schema_version: u32, root: &Path) -> PathBuf {
    let hash = repo_hash(root);
    cache_dir().join(format!("{tool}-v{schema_version}-{hash}.cache"))
}

/// Returns `(modified_nanoseconds_since_unix_epoch, file_size)` for a regular file.
pub fn file_mtime_size(path: &Path) -> Option<(u128, u64)> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Some((nanos, meta.len()))
}

/// Directory-memoized lookup of nearest manifest stamps (`go.mod`, `Cargo.toml`, `Package.swift`).
#[derive(Debug, Default)]
pub struct ManifestStampCache {
    seen: HashMap<(PathBuf, &'static str), Option<PathBuf>>,
    stamps: HashMap<PathBuf, (u128, u64)>,
}

impl ManifestStampCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn manifest_name(language: &str) -> Option<&'static str> {
        match language {
            "go" => Some("go.mod"),
            "rust" => Some("Cargo.toml"),
            "swift" => Some("Package.swift"),
            _ => None,
        }
    }

    /// Returns `(mtime_ns, size)` of the nearest language manifest for `file`, or `(0, 0)`.
    pub fn stamp_for(&mut self, file: &Path, language: &str) -> (u128, u64) {
        let Some(manifest_name) = Self::manifest_name(language) else {
            return (0, 0);
        };
        let Some(start_dir) = file.parent() else {
            return (0, 0);
        };
        let Some(manifest_path) = self.find_manifest(start_dir, manifest_name) else {
            return (0, 0);
        };
        if let Some(&stamp) = self.stamps.get(&manifest_path) {
            return stamp;
        }
        let stamp = file_mtime_size(&manifest_path).unwrap_or((0, 0));
        self.stamps.insert(manifest_path, stamp);
        stamp
    }

    fn find_manifest(&mut self, dir: &Path, name: &'static str) -> Option<PathBuf> {
        let key = (dir.to_path_buf(), name);
        if let Some(cached) = self.seen.get(&key) {
            return cached.clone();
        }
        let candidate = dir.join(name);
        let found = if candidate.is_file() {
            Some(candidate)
        } else {
            dir.parent()
                .and_then(|parent| self.find_manifest(parent, name))
        };
        self.seen.insert(key, found.clone());
        found
    }
}

/// Atomically writes cache bytes to `path`, ignoring best-effort I/O failures.
pub fn save_cache_file(path: &Path, bytes: &[u8]) {
    let _ = atomic_write(path, bytes);
}

/// Reads cache bytes from `path` if readable.
pub fn load_cache_file(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

/// String-interned binary encoder producing deterministic `UBC1` cache blobs.
#[derive(Debug, Default)]
pub struct CacheEncoder {
    strings: Vec<String>,
    string_ids: HashMap<String, u32>,
    payload: Vec<u8>,
}

impl CacheEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.string_ids.get(value) {
            return id;
        }
        let id = u32::try_from(self.strings.len()).expect("string table overflow");
        let owned = value.to_string();
        self.strings.push(owned.clone());
        self.string_ids.insert(owned, id);
        id
    }

    pub fn write_u8(&mut self, value: u8) {
        self.payload.push(value);
    }

    pub fn write_u32(&mut self, value: u32) {
        self.payload.extend_from_slice(&value.to_le_bytes());
    }

    pub fn write_u64(&mut self, value: u64) {
        self.payload.extend_from_slice(&value.to_le_bytes());
    }

    pub fn write_u128(&mut self, value: u128) {
        self.payload.extend_from_slice(&value.to_le_bytes());
    }

    pub fn write_usize(&mut self, value: usize) {
        let wide = u64::try_from(value).unwrap_or(u64::MAX);
        self.write_u64(wide);
    }

    pub fn write_bool(&mut self, value: bool) {
        self.write_u8(u8::from(value));
    }

    pub fn write_str(&mut self, value: &str) {
        let id = self.intern(value);
        self.write_u32(id);
    }

    /// Writes length-prefixed UTF-8 inline, bypassing the string-dedup table for unique strings.
    pub fn write_raw_str(&mut self, value: &str) {
        let len = u32::try_from(value.len()).expect("raw string length overflow");
        self.write_u32(len);
        self.payload.extend_from_slice(value.as_bytes());
    }

    pub fn write_opt_str(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_u8(1);
                self.write_str(s);
            }
            None => self.write_u8(0),
        }
    }

    pub fn write_opt_usize(&mut self, value: Option<usize>) {
        match value {
            Some(n) => {
                self.write_u8(1);
                self.write_usize(n);
            }
            None => self.write_u8(0),
        }
    }

    pub fn write_stamp(&mut self, stamp: FileStamp) {
        self.write_u128(stamp.mtime_ns);
        self.write_u64(stamp.size);
        self.write_u128(stamp.extra_mtime_ns);
        self.write_u64(stamp.extra_size);
    }

    pub fn finish(self, schema_version: u32) -> Vec<u8> {
        let mut table = Vec::new();
        for text in &self.strings {
            let len = u32::try_from(text.len()).expect("string length overflow");
            table.extend_from_slice(&len.to_le_bytes());
            table.extend_from_slice(text.as_bytes());
        }
        let table_len = u32::try_from(table.len()).expect("string table too large");
        let string_count = u32::try_from(self.strings.len()).expect("string count too large");

        let mut out = Vec::with_capacity(HEADER_LEN + table.len() + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&schema_version.to_le_bytes());
        out.extend_from_slice(&string_count.to_le_bytes());
        out.extend_from_slice(&table_len.to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Bounds-checked decoder for `UBC1` binary cache blobs.
#[derive(Debug)]
pub struct CacheDecoder<'a> {
    strings: Vec<&'a str>,
    payload: &'a [u8],
    pos: usize,
}

impl<'a> CacheDecoder<'a> {
    pub fn new(data: &'a [u8], expected_schema_version: u32) -> Option<Self> {
        if data.len() < HEADER_LEN || data[..4] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
        if version != expected_schema_version {
            return None;
        }
        let string_count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        let table_len = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
        let table_end = HEADER_LEN.checked_add(table_len)?;
        if table_end > data.len() {
            return None;
        }

        let mut strings = Vec::with_capacity(string_count.min(65_536));
        let mut cursor = HEADER_LEN;
        while strings.len() < string_count {
            if cursor + 4 > table_end {
                return None;
            }
            let len = u32::from_le_bytes(data[cursor..cursor + 4].try_into().ok()?) as usize;
            cursor += 4;
            let end = cursor.checked_add(len)?;
            if end > table_end {
                return None;
            }
            let text = std::str::from_utf8(&data[cursor..end]).ok()?;
            strings.push(text);
            cursor = end;
        }
        if cursor != table_end {
            return None;
        }

        Some(Self {
            strings,
            payload: &data[table_end..],
            pos: 0,
        })
    }

    pub fn is_eof(&self) -> bool {
        self.pos == self.payload.len()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        if end > self.payload.len() {
            return None;
        }
        let slice = &self.payload[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    pub fn read_u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub fn read_u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    pub fn read_u64(&mut self) -> Option<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(u64::from_le_bytes(bytes))
    }

    pub fn read_u128(&mut self) -> Option<u128> {
        let bytes: [u8; 16] = self.take(16)?.try_into().ok()?;
        Some(u128::from_le_bytes(bytes))
    }

    pub fn read_usize(&mut self) -> Option<usize> {
        usize::try_from(self.read_u64()?).ok()
    }

    pub fn read_bool(&mut self) -> Option<bool> {
        match self.read_u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn read_str(&mut self) -> Option<&'a str> {
        let id = self.read_u32()? as usize;
        self.strings.get(id).copied()
    }

    pub fn read_raw_str(&mut self) -> Option<&'a str> {
        let len = self.read_u32()? as usize;
        let slice = self.take(len)?;
        std::str::from_utf8(slice).ok()
    }

    pub fn read_opt_str(&mut self) -> Option<Option<&'a str>> {
        match self.read_u8()? {
            0 => Some(None),
            1 => Some(Some(self.read_str()?)),
            _ => None,
        }
    }

    pub fn read_opt_usize(&mut self) -> Option<Option<usize>> {
        match self.read_u8()? {
            0 => Some(None),
            1 => Some(Some(self.read_usize()?)),
            _ => None,
        }
    }

    pub fn read_stamp(&mut self) -> Option<FileStamp> {
        Some(FileStamp {
            mtime_ns: self.read_u128()?,
            size: self.read_u64()?,
            extra_mtime_ns: self.read_u128()?,
            extra_size: self.read_u64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ub_runtime_cache_test_{label}_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temp cache test dir");
        dir
    }

    #[test]
    fn encoder_decoder_round_trip_and_interns_duplicates() {
        let mut enc = CacheEncoder::new();
        let stamp = FileStamp {
            mtime_ns: 123_456_789_012_345,
            size: 4096,
            extra_mtime_ns: 99,
            extra_size: 120,
        };
        enc.write_stamp(stamp);
        enc.write_str("shared/path.ts");
        enc.write_str("shared/path.ts");
        enc.write_opt_str(Some("opt-value"));
        enc.write_opt_str(None);
        enc.write_opt_usize(Some(42));
        enc.write_opt_usize(None);
        enc.write_bool(true);
        enc.write_usize(777);
        enc.write_raw_str("raw-unique-id-αβγ");

        let bytes = enc.finish(3);
        let count_pos = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(count_pos, 2);

        let mut dec = CacheDecoder::new(&bytes, 3).expect("valid decode");
        assert_eq!(dec.read_stamp(), Some(stamp));
        assert_eq!(dec.read_str(), Some("shared/path.ts"));
        assert_eq!(dec.read_str(), Some("shared/path.ts"));
        assert_eq!(dec.read_opt_str(), Some(Some("opt-value")));
        assert_eq!(dec.read_opt_str(), Some(None));
        assert_eq!(dec.read_opt_usize(), Some(Some(42)));
        assert_eq!(dec.read_opt_usize(), Some(None));
        assert_eq!(dec.read_bool(), Some(true));
        assert_eq!(dec.read_usize(), Some(777));
        assert_eq!(dec.read_raw_str(), Some("raw-unique-id-αβγ"));
        assert!(dec.is_eof());
    }

    #[test]
    fn decoder_rejects_corrupt_truncated_or_wrong_version() {
        let mut enc = CacheEncoder::new();
        enc.write_str("hello");
        enc.write_u32(99);
        enc.write_raw_str("raw-tail");
        let bytes = enc.finish(5);

        assert!(CacheDecoder::new(&bytes, 4).is_none());
        assert!(CacheDecoder::new(&bytes[..4], 5).is_none());
        assert!(CacheDecoder::new(&bytes[..bytes.len() - 1], 5).is_some());
        let mut dec = CacheDecoder::new(&bytes[..bytes.len() - 1], 5).unwrap();
        assert_eq!(dec.read_str(), Some("hello"));
        assert_eq!(dec.read_u32(), Some(99));
        assert!(dec.read_raw_str().is_none());

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(CacheDecoder::new(&bad_magic, 5).is_none());

        let mut huge_table = bytes.clone();
        huge_table[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(CacheDecoder::new(&huge_table, 5).is_none());
    }

    #[test]
    fn file_mtime_size_and_manifest_stamp_detect_changes() {
        let dir = temp_dir("stamps");
        let sub = dir.join("nested").join("pkg");
        fs::create_dir_all(&sub).unwrap();
        let mod_file = dir.join("go.mod");
        fs::write(&mod_file, "module demo\n").unwrap();
        let src = sub.join("main.go");
        fs::write(&src, "package main\n").unwrap();

        let (m0, s0) = file_mtime_size(&src).expect("src stamp");
        assert!(s0 > 0);
        let mut cache = ManifestStampCache::new();
        let first = cache.stamp_for(&src, "go");
        assert_eq!(first, file_mtime_size(&mod_file).unwrap());
        assert_eq!(cache.stamp_for(&src, "typescript"), (0, 0));

        fs::write(&src, "package main\nfunc x() {}\n").unwrap();
        let (_m1, s1) = file_mtime_size(&src).expect("updated stamp");
        assert!(s1 > s0);
        let _ = m0;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_file_round_trip_and_missing_parent() {
        let dir = temp_dir("io");
        let path = dir.join("sub").join("tool.cache");
        save_cache_file(&path, b"payload");
        assert_eq!(
            load_cache_file(&path).as_deref(),
            Some(b"payload".as_slice())
        );
        assert!(load_cache_file(&dir.join("missing.cache")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_cache_dir_and_no_cache_in_isolated_subprocess() {
        const MARKER: &str = "UB_RUNTIME_CACHE_SUBPROC";
        if std::env::var_os(MARKER).is_some() {
            assert!(env_no_cache());
            let dir = cache_dir();
            assert_eq!(dir, PathBuf::from("custom-ub-cache"));
            let p = repo_cache_path("crapper", 1, Path::new("."));
            assert!(p.starts_with(&dir));
            return;
        }
        let exe = std::env::current_exe().expect("current test exe");
        let out = Command::new(exe)
            .args([
                "--exact",
                "cache::tests::env_cache_dir_and_no_cache_in_isolated_subprocess",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .env(ENV_CACHE_DIR, "custom-ub-cache")
            .env(ENV_NO_CACHE, " true ")
            .output()
            .expect("spawn cache env subproc");
        assert!(out.status.success());
    }
}
