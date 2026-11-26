//! Direct Cargo integration with `crapper-lib`, eliminating `crapper_link.py`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::cli::{CliProvider, run_with};
use crate::coverage::{
    CoverageProvider, GoProfileMap, NormalizedIndex, SwiftCoverageRecord, covered_from_pairs,
    jacoco_index, swift_hits,
};
use crate::engine::{EngineProvider, FormSpanMap};
use crate::functions::{form_digests, form_spans, sites_in_file};
use crate::model::Site;
use crate::path_util::{resolve_path, to_posix};
use crate::runner::{Command, test_plan_with, xcode_command};
use crate::sources::DiscoverProvider;

type GoSegmentTuple = (usize, usize, usize, u64);
type GoProfileCache = Option<NormalizedIndex<Vec<GoSegmentTuple>>>;

#[derive(Debug, Default)]
struct RootCoverageCache {
    lcov: Option<NormalizedIndex<HashSet<usize>>>,
    jacoco: Option<NormalizedIndex<HashSet<usize>>>,
    go_profile: Option<GoProfileCache>,
    form_html: Option<NormalizedIndex<HashSet<usize>>>,
    swift: Option<BTreeMap<String, crapper_lib::coverage::SwiftFile>>,
}

fn lcov_lines_index(root: &Path) -> NormalizedIndex<HashSet<usize>> {
    let raw = crapper_lib::coverage::load_lcov(root);
    NormalizedIndex::build(
        raw.into_iter().map(|(k, fc)| {
            let lines: HashSet<usize> = fc
                .lines
                .into_iter()
                .filter_map(|(ln, (cov, _))| if cov > 0 { Some(ln) } else { None })
                .collect();
            (k, lines)
        }),
        crapper_lib::coverage::normalize_path,
    )
}

fn form_html_lines_index(root: &Path) -> NormalizedIndex<HashSet<usize>> {
    let raw = crapper_lib::coverage::load_form_html(root);
    NormalizedIndex::build(
        raw.into_iter().map(|(k, map)| {
            let pairs: HashMap<usize, (usize, usize)> = map.into_iter().collect();
            let lines = covered_from_pairs(Some(&pairs)).unwrap_or_default();
            (k, lines)
        }),
        crapper_lib::coverage::normalize_path,
    )
}

/// Production provider delegating discovery, function extraction, coverage, and
/// runner planning directly to the `crapper-lib` Rust crate, with per-run
/// lazy coverage caching per project root.
#[derive(Debug, Default)]
pub struct CrapperBridge {
    cache: RwLock<HashMap<PathBuf, RootCoverageCache>>,
}

impl CrapperBridge {
    /// Create a fresh bridge instance with an empty per-run coverage cache.
    pub fn new() -> Self {
        Self::default()
    }
}

impl DiscoverProvider for CrapperBridge {
    fn language_of(&self, path: &Path) -> Option<&'static str> {
        crapper_lib::discover::language_of(path)
    }

    fn is_test_filename(&self, file_name: &str) -> bool {
        crapper_lib::discover::is_test_file(Path::new(file_name))
    }

    fn skip_dirs(&self) -> &[&str] {
        crapper_lib::discover::SKIP_DIRS
    }

    fn test_dirs(&self) -> &[&str] {
        crapper_lib::discover::TEST_DIRS
    }
}

impl CoverageProvider for CrapperBridge {
    fn normalize_path(&self, path: &str) -> String {
        crapper_lib::coverage::normalize_path(path)
    }

    fn go_import_name(&self, source_path: &Path, root: &Path) -> Option<String> {
        crapper_lib::coverage::go_import_name(source_path, root)
    }

    fn load_go_profile(&self, root: &Path) -> Option<GoProfileMap> {
        let profile = crapper_lib::coverage::load_go_profile(root)?;
        Some(
            profile
                .into_iter()
                .map(|(k, segs)| {
                    let tuples = segs
                        .into_iter()
                        .map(|s| (s.start_line, s.end_line, s.statements, s.hits))
                        .collect();
                    (k, tuples)
                })
                .collect(),
        )
    }

    fn load_form_html(&self, root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>> {
        crapper_lib::coverage::load_form_html(root)
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    }

    fn load_lcov(&self, root: &Path) -> HashMap<String, HashMap<usize, (usize, usize)>> {
        crapper_lib::coverage::load_lcov(root)
            .into_iter()
            .map(|(k, fc)| (k, fc.lines.into_iter().collect()))
            .collect()
    }

    fn lookup_swift(&self, root: &Path, source_path: &str) -> Option<SwiftCoverageRecord> {
        let swift = crapper_lib::coverage::load_swift(root);
        let found = crapper_lib::coverage::lookup(&swift, source_path, &[])?;
        Some(SwiftCoverageRecord {
            regions: found.regions.clone(),
            summaries: found.summaries.clone(),
        })
    }
}

/// Plan test command + cwd for Swift packages and Xcode projects using `crapper-lib`.
pub fn swift_test_plan(
    root: &Path,
    source: &Path,
    xcode_scheme: Option<&str>,
    xcode_destination: Option<&str>,
) -> (Command, PathBuf) {
    let packages =
        crapper_lib::runners::swift_packages(root, std::slice::from_ref(&source.to_path_buf()));
    if let Some(pkg) = packages.first() {
        return (
            Command::Argv(vec!["swift".into(), "test".into()]),
            pkg.clone(),
        );
    }
    let projects =
        crapper_lib::runners::xcode_projects(root, std::slice::from_ref(&source.to_path_buf()));
    let Some(project) = projects.first() else {
        return (
            Command::Argv(vec!["swift".into(), "test".into()]),
            root.to_path_buf(),
        );
    };
    let schemes = crapper_lib::runners::xcode_schemes(project);
    let cmd = xcode_command(project, xcode_scheme, xcode_destination, &schemes);
    let cwd = project
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.to_path_buf());
    (Command::Argv(cmd), cwd)
}

/// Resolve the test plan for any supported language using `crapper-lib`.
pub fn test_plan(
    root: &Path,
    source: &Path,
    language: &str,
    override_cmd: Option<&str>,
    xcode_scheme: Option<&str>,
    xcode_destination: Option<&str>,
) -> (Command, PathBuf) {
    test_plan_with(
        root,
        source,
        language,
        override_cmd,
        xcode_scheme,
        xcode_destination,
        swift_test_plan,
    )
}

impl EngineProvider for CrapperBridge {
    fn sites_in_file(&self, source: &str, path: &Path, root: &Path, file_key: &str) -> Vec<Site> {
        sites_in_file(source, path, root, file_key)
    }

    fn form_digests(
        &self,
        source: &str,
        path: &Path,
        root: &Path,
    ) -> HashMap<(String, String), String> {
        form_digests(source, path, root)
    }

    fn form_spans(&self, source: &str, path: &Path, root: &Path) -> FormSpanMap {
        form_spans(source, path, root)
    }

    fn test_plan(
        &self,
        root: &Path,
        source: &Path,
        language: &str,
        override_cmd: Option<&str>,
        xcode_scheme: Option<&str>,
        xcode_destination: Option<&str>,
    ) -> (Command, PathBuf) {
        test_plan(
            root,
            source,
            language,
            override_cmd,
            xcode_scheme,
            xcode_destination,
        )
    }
}

fn go_hits(
    profile_opt: &GoProfileCache,
    source_posix: &str,
    resolved_root: &Path,
) -> Option<HashSet<usize>> {
    let idx = profile_opt.as_ref()?;
    if idx.is_empty() {
        return None;
    }
    let name = crapper_lib::coverage::go_import_name(Path::new(source_posix), resolved_root);
    let segments = idx.lookup(
        source_posix,
        name.as_deref(),
        crapper_lib::coverage::normalize_path,
    )?;
    let mut covered = HashSet::new();
    for &(start, end, _statements, hits) in segments {
        if hits == 0 {
            continue;
        }
        for line in start..=end {
            covered.insert(line);
        }
    }
    Some(covered)
}

fn lookup_cached(
    entry: &RootCoverageCache,
    resolved_root: &Path,
    source_posix: &str,
    language: &str,
) -> Option<Option<HashSet<usize>>> {
    match language {
        "java" => entry.jacoco.as_ref().map(|idx| {
            idx.lookup(source_posix, None, crapper_lib::coverage::normalize_path)
                .cloned()
        }),
        "go" => entry
            .go_profile
            .as_ref()
            .map(|opt| go_hits(opt, source_posix, resolved_root)),
        "swift" => entry.swift.as_ref().map(|swift_map| {
            let found = crapper_lib::coverage::lookup(swift_map, source_posix, &[])?;
            Some(swift_hits(&SwiftCoverageRecord {
                regions: found.regions.clone(),
                summaries: found.summaries.clone(),
            }))
        }),
        "clojure" => {
            let html_idx = entry.form_html.as_ref()?;
            if let Some(lines) =
                html_idx.lookup(source_posix, None, crapper_lib::coverage::normalize_path)
            {
                return Some(Some(lines.clone()));
            }
            entry.lcov.as_ref().map(|idx| {
                idx.lookup(source_posix, None, crapper_lib::coverage::normalize_path)
                    .cloned()
            })
        }
        _ => entry.lcov.as_ref().map(|idx| {
            idx.lookup(source_posix, None, crapper_lib::coverage::normalize_path)
                .cloned()
        }),
    }
}

fn ensure_cached_slots(entry: &mut RootCoverageCache, resolved_root: &Path, language: &str) {
    match language {
        "java" => {
            entry.jacoco.get_or_insert_with(|| {
                NormalizedIndex::build(
                    jacoco_index(resolved_root),
                    crapper_lib::coverage::normalize_path,
                )
            });
        }
        "go" => {
            entry.go_profile.get_or_insert_with(|| {
                let raw = crapper_lib::coverage::load_go_profile(resolved_root)?;
                Some(NormalizedIndex::build(
                    raw.into_iter().map(|(k, segs)| {
                        let tuples = segs
                            .into_iter()
                            .map(|s| (s.start_line, s.end_line, s.statements, s.hits))
                            .collect();
                        (k, tuples)
                    }),
                    crapper_lib::coverage::normalize_path,
                ))
            });
        }
        "swift" => {
            entry
                .swift
                .get_or_insert_with(|| crapper_lib::coverage::load_swift(resolved_root));
        }
        "clojure" => {
            entry
                .form_html
                .get_or_insert_with(|| form_html_lines_index(resolved_root));
            entry
                .lcov
                .get_or_insert_with(|| lcov_lines_index(resolved_root));
        }
        _ => {
            entry
                .lcov
                .get_or_insert_with(|| lcov_lines_index(resolved_root));
        }
    }
}

impl CliProvider for CrapperBridge {
    fn run_coverage(&self, root: &Path, files: &[PathBuf]) -> i32 {
        crapper_lib::runners::run_coverage(root, files, None)
    }

    fn covered_lines(&self, root: &Path, path: &Path, language: &str) -> Option<HashSet<usize>> {
        let resolved_root = resolve_path(root);
        let source_posix = to_posix(&resolve_path(path));
        {
            let guard = self.cache.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.get(&resolved_root)
                && let Some(cached) = lookup_cached(entry, &resolved_root, &source_posix, language)
            {
                return cached;
            }
        }
        let mut guard = self.cache.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(resolved_root.clone()).or_default();
        ensure_cached_slots(entry, &resolved_root, language);
        lookup_cached(entry, &resolved_root, &source_posix, language).flatten()
    }
}

/// Run the full `mutator` CLI using `CrapperBridge`.
pub fn run_cli(args: &[String]) -> i32 {
    run_with(args, &CrapperBridge::new())
}

/// Compute covered lines for a source file using `CrapperBridge`.
pub fn covered_lines(root: &Path, source_path: &Path, language: &str) -> Option<HashSet<usize>> {
    let resolved_root = resolve_path(root);
    CrapperBridge::new().covered_lines(&resolved_root, source_path, language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_test_plan_prefers_package_over_xcode_project() {
        let root = std::env::temp_dir().join(format!("mutator_swift_{}", uuid::Uuid::new_v4()));
        let pkg_dir = root.join("libs").join("Demo");
        let pkg_src = pkg_dir.join("Sources").join("Demo").join("Box.swift");
        std::fs::create_dir_all(pkg_src.parent().unwrap()).unwrap();
        std::fs::write(
            pkg_dir.join("Package.swift"),
            "// swift-tools-version:5.9\n",
        )
        .unwrap();
        std::fs::write(&pkg_src, "func open() -> Int { 1 }\n").unwrap();

        let proj_dir = root.join("App.xcodeproj");
        std::fs::create_dir_all(proj_dir.join("xcshareddata").join("xcschemes")).unwrap();
        std::fs::write(
            proj_dir
                .join("xcshareddata")
                .join("xcschemes")
                .join("App.xcscheme"),
            "<Scheme/>\n",
        )
        .unwrap();
        let app_src = root.join("App").join("ContentView.swift");
        std::fs::create_dir_all(app_src.parent().unwrap()).unwrap();
        std::fs::write(&app_src, "func body() -> Int { 1 }\n").unwrap();

        let (pkg_cmd, pkg_cwd) = test_plan(&root, &pkg_src, "swift", None, None, None);
        assert_eq!(pkg_cmd, Command::Argv(vec!["swift".into(), "test".into()]));
        assert_eq!(pkg_cwd, resolve_path(&pkg_dir));

        let (app_cmd, app_cwd) = test_plan(
            &root,
            &app_src,
            "swift",
            None,
            None,
            Some("platform=iOS Simulator,name=iPhone 16"),
        );
        assert_eq!(
            app_cmd,
            Command::Argv(vec![
                "xcodebuild".into(),
                "test".into(),
                "-project".into(),
                "App.xcodeproj".into(),
                "-scheme".into(),
                "App".into(),
                "-destination".into(),
                "platform=iOS Simulator,name=iPhone 16".into(),
            ])
        );
        assert_eq!(app_cwd, resolve_path(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn end_to_end_cli_runs_exit_codes_and_cleans_workers() {
        let root = std::env::temp_dir().join(format!("mutator_e2e_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let src = root.join("src").join("demo.py");
        std::fs::write(
            &src,
            "def add(a, b):\r\n    if a > 0:\r\n        return a + b\r\n    return 0\r\n",
        )
        .unwrap();
        let original_bytes = std::fs::read(&src).unwrap();

        let py = if cfg!(windows) { "python" } else { "python3" };
        let test_cmd = format!(
            "{py} -c \"import sys; sys.path.insert(0, 'src'); from demo import add; raise SystemExit(0 if add(2, 3) == 5 else 1)\""
        );
        let root_str = root.to_string_lossy().into_owned();

        let scan_code = run_cli(&[
            "--scan".into(),
            "--no-coverage".into(),
            "--root".into(),
            root_str.clone(),
            "src/demo.py".into(),
        ]);
        assert_eq!(scan_code, 0);
        assert!(!root.join(".metrics").exists());

        let killed_code = run_cli(&[
            "--no-coverage".into(),
            "--mutate-all".into(),
            "--lines".into(),
            "3".into(),
            "--max-workers".into(),
            "1".into(),
            "--root".into(),
            root_str.clone(),
            "--test-command".into(),
            test_cmd.clone(),
            "src/demo.py".into(),
        ]);
        assert_eq!(killed_code, 0);
        assert!(!root.join("target").join("mutation-workers").exists());

        let survived_code = run_cli(&[
            "--no-coverage".into(),
            "--mutate-all".into(),
            "--max-workers".into(),
            "1".into(),
            "--root".into(),
            root_str.clone(),
            "--test-command".into(),
            test_cmd,
            "src/demo.py".into(),
        ]);
        assert_eq!(survived_code, 3);
        assert_eq!(std::fs::read(&src).unwrap(), original_bytes);
        assert!(!root.join("target").join("mutation-workers").exists());
        let snapshot = root.join(".metrics").join("mutate").join("demo.edn");
        assert!(snapshot.is_file());
        let snap_text = std::fs::read_to_string(&snapshot).unwrap();
        assert!(snap_text.contains(":namespace \"demo\""));
        assert!(snap_text.contains(":id \"defn/add\""));

        let baseline_fail = run_cli(&[
            "--no-coverage".into(),
            "--mutate-all".into(),
            "--root".into(),
            root_str,
            "--test-command".into(),
            format!("{py} -c \"raise SystemExit(1)\""),
            "src/demo.py".into(),
        ]);
        assert_eq!(baseline_fail, 2);

        let _ = std::fs::remove_dir_all(&root);
    }
}
