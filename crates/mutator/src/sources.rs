//! Choose the source files a mutation run will consider.

use std::collections::{BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::path_util::{resolve_path, to_posix};

const SRC_TEST_DIRS: &[&str] = &["test", "tests"];

/// Abstraction over `crapper::discover` so `sources` can be tested independently
/// and wired straight to `crapper-core` once linked.
pub trait DiscoverProvider {
    fn language_of(&self, path: &Path) -> Option<&'static str>;
    fn is_test_filename(&self, file_name: &str) -> bool;
    fn skip_dirs(&self) -> &[&str];
    fn test_dirs(&self) -> &[&str];
}

fn normal_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// True when `relative` sits under a root test directory or `src/test` / `src/tests`.
pub fn is_test_path(relative: &Path, test_dirs: &HashSet<&str>) -> bool {
    let parts = normal_parts(relative);
    let Some(first) = parts.first() else {
        return false;
    };
    if test_dirs.contains(first.as_str()) {
        return true;
    }
    parts.len() >= 2 && first == "src" && SRC_TEST_DIRS.contains(&parts[1].as_str())
}

fn swift_test_relative(relative: &Path) -> bool {
    if relative.extension().and_then(|e| e.to_str()) != Some("swift") {
        return false;
    }
    let parts = normal_parts(relative);
    if parts.len() <= 1 {
        return false;
    }
    parts[..parts.len() - 1]
        .iter()
        .any(|part| part.ends_with("Tests"))
}

fn relative_to_root(path: &Path, root: &Path) -> Option<PathBuf> {
    let resolved_path = resolve_path(path);
    let resolved_root = resolve_path(root);
    resolved_path
        .strip_prefix(&resolved_root)
        .ok()
        .map(Path::to_path_buf)
}

/// True when `path` is a non-test source file inside `root` in a supported language.
pub fn is_mutable_source<D: DiscoverProvider>(path: &Path, root: &Path, discover: &D) -> bool {
    if discover.language_of(path).is_none() {
        return false;
    }
    let Some(relative) = relative_to_root(path, root) else {
        return false;
    };
    let skip: HashSet<&str> = discover.skip_dirs().iter().copied().collect();
    if normal_parts(&relative)
        .iter()
        .any(|part| skip.contains(part.as_str()))
    {
        return false;
    }
    let test_dirs: HashSet<&str> = discover.test_dirs().iter().copied().collect();
    if is_test_path(&relative, &test_dirs) {
        return false;
    }
    if swift_test_relative(&relative) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    !discover.is_test_filename(name)
}

/// True when `path` is a supported-language file excluded because it is a test file.
pub fn is_skipped_test<D: DiscoverProvider>(path: &Path, root: &Path, discover: &D) -> bool {
    if discover.language_of(path).is_none() {
        return false;
    }
    let test_dirs: HashSet<&str> = discover.test_dirs().iter().copied().collect();
    if let Some(relative) = relative_to_root(path, root)
        && (is_test_path(&relative, &test_dirs) || swift_test_relative(&relative))
    {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    discover.is_test_filename(name)
}

pub fn should_enter_dir(
    current: &Path,
    name: &str,
    project_root: &Path,
    skip_dirs: &HashSet<&str>,
    test_dirs: &HashSet<&str>,
) -> bool {
    if skip_dirs.contains(name) {
        return false;
    }
    match relative_to_root(&current.join(name), project_root) {
        None => true,
        Some(relative) => !is_test_path(&relative, test_dirs),
    }
}

fn walk_dir<D: DiscoverProvider>(
    current: &Path,
    project_root: &Path,
    skip_dirs: &HashSet<&str>,
    test_dirs: &HashSet<&str>,
    discover: &D,
    files: &mut BTreeSet<(String, PathBuf)>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if file_type.is_dir() {
            if should_enter_dir(
                current,
                name_str.as_ref(),
                project_root,
                skip_dirs,
                test_dirs,
            ) {
                walk_dir(&path, project_root, skip_dirs, test_dirs, discover, files);
            }
        } else if file_type.is_file() || file_type.is_symlink() {
            let resolved = resolve_path(&path);
            if resolved.is_file() && is_mutable_source(&resolved, project_root, discover) {
                files.insert((to_posix(&resolved), resolved));
            }
        }
    }
}

/// Collect sorted mutable source files under `roots` within `project_root`.
pub fn iter_source_files<D: DiscoverProvider>(
    roots: &[PathBuf],
    project_root: &Path,
    discover: &D,
) -> Vec<PathBuf> {
    let skip_dirs: HashSet<&str> = discover.skip_dirs().iter().copied().collect();
    let test_dirs: HashSet<&str> = discover.test_dirs().iter().copied().collect();
    let project_root = resolve_path(project_root);
    let mut files: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for start in roots {
        let resolved_start = resolve_path(start);
        if resolved_start.is_file() {
            if is_mutable_source(&resolved_start, &project_root, discover) {
                files.insert((to_posix(&resolved_start), resolved_start));
            }
            continue;
        }
        if !resolved_start.exists() {
            continue;
        }
        walk_dir(
            &resolved_start,
            &project_root,
            &skip_dirs,
            &test_dirs,
            discover,
            &mut files,
        );
    }
    files.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDiscover;

    impl DiscoverProvider for TestDiscover {
        fn language_of(&self, path: &Path) -> Option<&'static str> {
            let name = path.file_name()?.to_str()?;
            if name.ends_with(".d.ts") {
                return None;
            }
            match path.extension()?.to_str()? {
                "py" => Some("python"),
                "java" => Some("java"),
                "go" => Some("go"),
                "ts" => Some("typescript"),
                "swift" => Some("swift"),
                _ => None,
            }
        }
        fn is_test_filename(&self, name: &str) -> bool {
            name == "conftest.py"
                || name.ends_with("_test.py")
                || (name.starts_with("test_") && name.ends_with(".py"))
                || name.ends_with("_test.go")
                || name.ends_with(".test.ts")
                || name.ends_with(".spec.ts")
                || name.ends_with("Tests.swift")
        }
        fn skip_dirs(&self) -> &[&str] {
            &[
                ".git",
                ".venv",
                "venv",
                "node_modules",
                "target",
                "build",
                ".build",
                ".swiftpm",
                "DerivedData",
            ]
        }
        fn test_dirs(&self) -> &[&str] {
            &["test", "tests", "spec", "specs", "__tests__"]
        }
    }

    #[test]
    fn walk_skips_root_and_src_test_but_keeps_deeper_test_dirs() {
        let root = std::env::temp_dir().join(format!("mutator_sources_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let cases = [
            ("test/Foo.java", false),
            ("tests/foo.py", false),
            ("spec/foo.py", false),
            ("specs/foo.py", false),
            ("__tests__/foo.py", false),
            ("src/test/java/com/example/Foo.java", false),
            ("src/tests/foo.py", false),
            ("src/agnesse/slim/test/SymbolManagingTableTable.java", true),
            ("src/foo/tests/bar.py", true),
            ("src/pkg/spec/foo.py", true),
            ("src/demo.py", true),
            ("lib/test/deep.py", true),
            ("src/pkg/node_modules/foo.py", false),
            ("target/generated/Foo.java", false),
            ("build/out.py", false),
            ("src/pkg/foo_test.go", false),
            ("src/pkg/test_foo.py", false),
            ("src/pkg/foo_test.py", false),
            ("src/pkg/foo.test.ts", false),
            ("src/pkg/foo.spec.ts", false),
            ("src/pkg/conftest.py", false),
            ("Tests/DemoTests/Board.swift", false),
            ("AppTests/Board.swift", false),
            ("Sources/Demo/BoxTests.swift", false),
            ("Sources/Demo/Box.swift", true),
            (".build/debug/Generated.swift", false),
            (".swiftpm/cache/Generated.swift", false),
            ("DerivedData/App.swift", false),
            ("src/pkg/Tests/Hidden.swift", false),
            ("src/pkg/test/Kept.swift", true),
        ];
        for (rel, _) in cases {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "def place():\n    return 1\n").unwrap();
        }
        let chosen: HashSet<String> =
            iter_source_files(std::slice::from_ref(&root), &root, &TestDiscover)
                .into_iter()
                .map(|p| crate::path_util::relative_posix(&p, &root).unwrap())
                .collect();
        let expected: HashSet<String> = cases
            .iter()
            .filter(|(_, keep)| *keep)
            .map(|(rel, _)| (*rel).to_string())
            .collect();
        assert_eq!(chosen, expected);

        assert!(!is_test_path(Path::new(""), &HashSet::from(["test"])));
        assert!(is_test_path(
            Path::new("src/test"),
            &HashSet::from(["test", "tests"])
        ));
        assert!(is_test_path(
            Path::new("src/tests"),
            &HashSet::from(["test", "tests"])
        ));
        assert!(!is_test_path(
            Path::new("src"),
            &HashSet::from(["test", "tests"])
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
