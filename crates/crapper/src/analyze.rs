use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use ub_runtime::{FileStamp, ManifestStampCache};

use crate::cache::{CachedFile, cache_path, load_cache, merge_cache, save_cache};
use crate::coverage::CoverageBundle;
use crate::crap::{make_entry, sort_entries};
use crate::discover::language_of;
use crate::languages::functions_in_file;
use crate::model::{Entry, Function};
use crate::path_util::{resolve_path, to_posix};

fn coverage_for(bundle: Option<&CoverageBundle>, function: &Function) -> Option<f64> {
    let b = bundle?;
    Some(b.percent_for(function).unwrap_or(0.0))
}

fn source_path(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => to_posix(rel),
        Err(_) => to_posix(file),
    }
}

struct MissTarget {
    resolved: PathBuf,
    key: String,
    language: &'static str,
    relative: String,
    stamp: Option<FileStamp>,
}

struct ParsedMiss {
    key: String,
    stamp: Option<FileStamp>,
    language: &'static str,
    relative: String,
    functions: Vec<Function>,
}

struct CachePartition {
    hit_functions: Vec<Function>,
    hit_keys: Vec<String>,
    misses: Vec<MissTarget>,
    cached_map: HashMap<String, CachedFile>,
    cache_file: Option<PathBuf>,
}

struct PreparedFile {
    idx: usize,
    key: String,
    language: &'static str,
    relative: String,
    stamp: Option<FileStamp>,
}

fn partition_files(
    resolved_files: &[PathBuf],
    root: &Path,
    use_cache: bool,
    cache_override: Option<&Path>,
) -> CachePartition {
    let mut hit_functions = Vec::new();
    let mut hit_keys = Vec::new();
    let mut misses = Vec::new();

    if !use_cache {
        for resolved in resolved_files {
            let Some(language) = language_of(resolved) else {
                continue;
            };
            misses.push(MissTarget {
                resolved: resolved.clone(),
                key: String::new(),
                language,
                relative: source_path(resolved, root),
                stamp: None,
            });
        }
        return CachePartition {
            hit_functions,
            hit_keys,
            misses,
            cached_map: HashMap::new(),
            cache_file: None,
        };
    }

    let cache_file = cache_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cache_path(root));
    let mut cached_map = load_cache(&cache_file);

    let prepared: Vec<PreparedFile> = resolved_files
        .par_iter()
        .enumerate()
        .map_init(
            ManifestStampCache::new,
            |manifest_cache, (idx, resolved)| {
                let language = language_of(resolved)?;
                let relative = source_path(resolved, root);
                let key = to_posix(resolved);
                let (extra_mtime_ns, extra_size) = manifest_cache.stamp_for(resolved, language);
                let stamp = FileStamp::from_paths(resolved, extra_mtime_ns, extra_size);
                Some(PreparedFile {
                    idx,
                    key,
                    language,
                    relative,
                    stamp,
                })
            },
        )
        .filter_map(std::convert::identity)
        .collect();

    let mut pending_hits: Vec<String> = Vec::with_capacity(prepared.len());
    let mut seen_hits: HashSet<&str> = HashSet::with_capacity(prepared.len());
    let mut hits_unique = true;

    for item in &prepared {
        let is_hit = item.stamp.is_some_and(|st| {
            cached_map.get(&item.key).is_some_and(|cached| {
                cached.stamp == st
                    && cached.language == item.language
                    && cached.relative == item.relative
            })
        });
        if is_hit {
            if !seen_hits.insert(item.key.as_str()) {
                hits_unique = false;
            }
        } else {
            misses.push(MissTarget {
                resolved: resolved_files[item.idx].clone(),
                key: item.key.clone(),
                language: item.language,
                relative: item.relative.clone(),
                stamp: item.stamp,
            });
        }
    }

    for item in prepared {
        let was_hit = item.stamp.is_some_and(|st| {
            cached_map.get(&item.key).is_some_and(|cached| {
                cached.stamp == st
                    && cached.language == item.language
                    && cached.relative == item.relative
            })
        });
        if was_hit {
            pending_hits.push(item.key);
        }
    }

    if misses.is_empty() && hits_unique {
        for key in pending_hits {
            if let Some(cached) = cached_map.remove(&key) {
                hit_keys.push(key);
                hit_functions.extend(cached.functions);
            }
        }
    } else {
        for key in pending_hits {
            if let Some(cached) = cached_map.get(&key) {
                hit_keys.push(key);
                hit_functions.extend(cached.functions.iter().cloned());
            }
        }
    }

    CachePartition {
        hit_functions,
        hit_keys,
        misses,
        cached_map,
        cache_file: Some(cache_file),
    }
}

pub fn analyze_files(
    files: &[PathBuf],
    project_root: &Path,
    bundle: Option<&mut CoverageBundle>,
) -> Vec<Entry> {
    analyze_files_with_cache(files, project_root, bundle, false)
}

pub fn analyze_files_with_cache(
    files: &[PathBuf],
    project_root: &Path,
    bundle: Option<&mut CoverageBundle>,
    use_cache: bool,
) -> Vec<Entry> {
    analyze_files_impl(files, project_root, bundle, use_cache, None, false)
}

pub(crate) fn analyze_resolved_files_with_cache(
    files: &[PathBuf],
    project_root: &Path,
    bundle: Option<&mut CoverageBundle>,
    use_cache: bool,
) -> Vec<Entry> {
    analyze_files_impl(files, project_root, bundle, use_cache, None, true)
}

fn analyze_files_impl(
    files: &[PathBuf],
    project_root: &Path,
    mut bundle: Option<&mut CoverageBundle>,
    use_cache: bool,
    cache_override: Option<&Path>,
    paths_resolved: bool,
) -> Vec<Entry> {
    let root = resolve_path(project_root);
    let root_str = root.to_string_lossy().into_owned();
    let resolved_files: Cow<'_, [PathBuf]> = if paths_resolved {
        Cow::Borrowed(files)
    } else {
        Cow::Owned(files.par_iter().map(|f| resolve_path(f)).collect())
    };
    if let Some(b) = bundle.as_deref_mut() {
        let paths: Vec<String> = resolved_files
            .iter()
            .map(|resolved| source_path(resolved, &root))
            .collect();
        b.bind_sources(&paths);
    }
    if resolved_files.is_empty() {
        return Vec::new();
    }

    let partition = partition_files(&resolved_files, &root, use_cache, cache_override);
    let parsed_misses: Vec<ParsedMiss> = partition
        .misses
        .par_iter()
        .filter_map(|target| {
            let bytes = fs::read(&target.resolved).ok()?;
            let source = String::from_utf8_lossy(&bytes);
            let functions = functions_in_file(
                target.language,
                &source,
                &target.relative,
                Some(root_str.as_str()),
            );
            Some(ParsedMiss {
                key: target.key.clone(),
                stamp: target.stamp,
                language: target.language,
                relative: target.relative.clone(),
                functions,
            })
        })
        .collect();

    let mut all_functions = partition.hit_functions;
    let mut cache_updates = Vec::new();
    for miss in parsed_misses {
        if use_cache && let Some(stamp) = miss.stamp {
            cache_updates.push((
                miss.key,
                CachedFile {
                    stamp,
                    language: miss.language.to_string(),
                    relative: miss.relative,
                    functions: miss.functions.clone(),
                },
            ));
        }
        all_functions.extend(miss.functions);
    }

    if use_cache
        && !cache_updates.is_empty()
        && let Some(cache_file) = partition.cache_file
    {
        let merged = merge_cache(partition.cached_map, cache_updates, &partition.hit_keys);
        save_cache(&cache_file, &merged);
    }

    let bundle_ref = bundle.as_deref();
    let entries: Vec<Entry> = all_functions
        .par_iter()
        .map(|function| make_entry(function, coverage_for(bundle_ref, function)))
        .collect();
    sort_entries(&entries)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    const SUBPROCESS_MARKER: &str = "CRAPPER_ANALYZE_CACHE_SUBPROC";
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
                "crapper-analyze-{label}-{}-{nanos}-{id}",
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

    fn set_past_mtime(path: &Path, secs_ago: u64) {
        let past = SystemTime::now() - Duration::from_secs(secs_ago);
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open file for set_modified");
        file.set_modified(past).expect("set past modified time");
    }

    fn modified_time(path: &Path) -> SystemTime {
        fs::metadata(path)
            .expect("read metadata")
            .modified()
            .expect("read modified time")
    }

    #[test]
    fn cache_hit_matches_uncached_and_avoids_rewriting_cache() {
        let dir = TestDir::new("hit-no-write");
        let cache_file = dir.0.join("cache").join("crapper.cache");
        let py_file = dir.0.join("src").join("app.py");
        fs::create_dir_all(py_file.parent().unwrap()).unwrap();
        fs::write(
            &py_file,
            "def branchy(x):\n    if x > 0:\n        return 1\n    return 0\n",
        )
        .unwrap();

        let files = vec![py_file.clone()];
        let expected = analyze_files(&files, &dir.0, None);
        let first = analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(first, expected);
        assert!(cache_file.is_file());

        set_past_mtime(&cache_file, 120);
        let mtime_before = modified_time(&cache_file);

        // Tamper cached function name while keeping FileStamp identical to prove cache hit path is used.
        let key = to_posix(&resolve_path(&py_file));
        let mut records = load_cache(&cache_file);
        records.get_mut(&key).unwrap().functions[0].name = "from_cache_marker".to_string();
        save_cache(&cache_file, &records);
        set_past_mtime(&cache_file, 120);
        let marker_mtime = modified_time(&cache_file);

        let second = analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "from_cache_marker");
        assert_eq!(modified_time(&cache_file), marker_mtime);
        assert_ne!(mtime_before, SystemTime::UNIX_EPOCH);

        // `use_cache == false` must bypass cache reads and writes.
        let uncached = analyze_files_impl(&files, &dir.0, None, false, Some(&cache_file), false);
        assert_eq!(uncached, expected);
        assert_eq!(modified_time(&cache_file), marker_mtime);
    }

    #[test]
    fn cache_invalidates_on_source_and_manifest_changes() {
        let dir = TestDir::new("invalidate");
        let cache_file = dir.0.join("cache").join("crapper.cache");
        let go_mod = dir.0.join("go.mod");
        let go_file = dir.0.join("pkg").join("calc.go");
        fs::create_dir_all(go_file.parent().unwrap()).unwrap();
        fs::write(&go_mod, "module example.com/one\n").unwrap();
        fs::write(
            &go_file,
            "package pkg\nfunc Alpha(x int) int {\n\tif x > 0 {\n\t\treturn 1\n\t}\n\treturn 0\n}\n",
        )
        .unwrap();

        let files = vec![go_file.clone()];
        let initial = analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].namespace, "example.com/one/pkg");

        // Update go.mod only: manifest stamp invalidates cached Go namespace.
        fs::write(&go_mod, "module example.com/two_updated\n").unwrap();
        let after_manifest =
            analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(after_manifest.len(), 1);
        assert_eq!(after_manifest[0].namespace, "example.com/two_updated/pkg");
        assert_eq!(after_manifest, analyze_files(&files, &dir.0, None));

        // Update source file only: source stamp invalidates cached AST functions.
        fs::write(
            &go_file,
            "package pkg\nfunc Alpha(x int) int {\n\treturn x\n}\nfunc Beta(x int) int {\n\treturn x + 1\n}\n",
        )
        .unwrap();
        let after_source = analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(after_source.len(), 2);
        assert_eq!(after_source, analyze_files(&files, &dir.0, None));

        // Check Cargo.toml manifest invalidation as well.
        let rust_crate = dir.0.join("rust_mod");
        let cargo_toml = rust_crate.join("Cargo.toml");
        let rs_file = rust_crate.join("src").join("lib.rs");
        fs::create_dir_all(rs_file.parent().unwrap()).unwrap();
        fs::write(
            &cargo_toml,
            "[package]\nname = \"crate_one\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(&rs_file, "pub fn helper() -> i32 { 1 }\n").unwrap();

        let rs_files = vec![rs_file.clone()];
        let rs_first = analyze_files_impl(&rs_files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(rs_first[0].namespace, "crate_one");

        fs::write(
            &cargo_toml,
            "[package]\nname = \"crate_renamed\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let rs_second = analyze_files_impl(&rs_files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(rs_second[0].namespace, "crate_renamed");
        assert_eq!(rs_second, analyze_files(&rs_files, &dir.0, None));
    }

    #[test]
    fn corrupt_cache_file_is_ignored_and_repaired_without_panicking() {
        let dir = TestDir::new("corrupt");
        let cache_file = dir.0.join("cache").join("crapper.cache");
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, b"CORRUPT-DATA-BYTES").unwrap();

        let py_file = dir.0.join("app.py");
        fs::write(&py_file, "def fn_a(x):\n    return x\n").unwrap();
        let files = vec![py_file];

        let expected = analyze_files(&files, &dir.0, None);
        let actual = analyze_files_impl(&files, &dir.0, None, true, Some(&cache_file), false);
        assert_eq!(actual, expected);
        assert!(!load_cache(&cache_file).is_empty());
    }

    #[test]
    fn analyze_files_never_writes_cache_and_cache_dir_env_works_in_subprocess() {
        const CACHE_DIR_ENV: &str = "UB_TOOLS_CACHE_DIR";
        if std::env::var_os(SUBPROCESS_MARKER).is_some() {
            let dir = TestDir::new("subproc");
            let cache_dir = PathBuf::from(std::env::var_os(CACHE_DIR_ENV).expect("cache dir"));
            let py_file = dir.0.join("app.py");
            fs::write(&py_file, "def f(x):\n    return x + 1\n").unwrap();
            let files = vec![py_file];

            let uncached = analyze_files(&files, &dir.0, None);
            assert_eq!(uncached.len(), 1);
            assert!(!cache_path(&resolve_path(&dir.0)).exists());

            let cached = analyze_files_with_cache(&files, &dir.0, None, true);
            assert_eq!(cached, uncached);
            let expected_cache = cache_path(&resolve_path(&dir.0));
            assert!(expected_cache.starts_with(&cache_dir));
            assert!(expected_cache.is_file());
            return;
        }

        let dir = TestDir::new("subproc-host");
        let cache_dir = dir.0.join("ub-cache");
        let exe = std::env::current_exe().expect("current test exe");
        let out = Command::new(exe)
            .args([
                "--exact",
                "analyze::tests::analyze_files_never_writes_cache_and_cache_dir_env_works_in_subprocess",
                "--nocapture",
            ])
            .env(SUBPROCESS_MARKER, "1")
            .env(CACHE_DIR_ENV, &cache_dir)
            .env_remove("UB_TOOLS_NO_CACHE")
            .output()
            .expect("spawn analyze cache subprocess");
        assert!(
            out.status.success(),
            "subprocess failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
