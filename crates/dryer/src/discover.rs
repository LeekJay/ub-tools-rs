//! Find source files and detect which language each one is written in.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".venv",
    "venv",
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    ".metrics",
    "testdata",
    "__pycache__",
    ".clj-kondo",
];

pub const TEST_DIRS: &[&str] = &["test", "tests", "spec", "specs", "__tests__", "Tests"];

const TEST_SUFFIXES: &[&str] = &[
    "_test.go",
    "_test.clj",
    "_test.cljc",
    "_test.cljs",
    "_test.cljd",
    "_test.bb",
    ".test.ts",
    ".spec.ts",
    ".test.tsx",
    ".spec.tsx",
    ".test.mts",
    ".spec.mts",
    ".test.cts",
    ".spec.cts",
    ".test.vue",
    ".spec.vue",
    "_test.py",
    "Tests.swift",
];

/// Map a source path to its dryer language name, or `None` for non-source files.
pub fn language_of(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".d.ts") {
        return None;
    }
    let ext = path.extension()?.to_str()?;
    match ext {
        "clj" | "cljc" | "cljs" | "cljd" | "bb" => Some("clojure"),
        "java" => Some("java"),
        "go" => Some("go"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "rs" => Some("rust"),
        "py" => Some("python"),
        "swift" => Some("swift"),
        "vue" => Some("vue"),
        _ => None,
    }
}

/// Return true when `path` matches one of dryer's test-file or test-directory rules.
pub fn is_test_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name == "conftest.py" || TEST_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
            return true;
        }
        if name.starts_with("test_") && name.ends_with(".py") {
            return true;
        }
    }
    path.components().any(|component| match component {
        Component::Normal(part) => part.to_str().is_some_and(|name| TEST_DIRS.contains(&name)),
        _ => false,
    })
}

pub fn is_skipped_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || TEST_DIRS.contains(&name)
}

/// Canonicalize `path` when it exists, stripping Windows `\\?\` prefixes to match Python's `Path.resolve()`.
pub fn resolve_path(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());
    strip_windows_verbatim_prefix(resolved)
}

fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(text) = path.to_str() {
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = text.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// Format a path using forward slashes regardless of host separator.
pub fn to_posix(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn walk_root(root: &Path, out: &mut Vec<(PathBuf, bool)>) {
    let resolved_root = resolve_path(root);
    if !resolved_root.exists() {
        return;
    }
    if resolved_root.is_file() {
        out.push((resolved_root, false));
        return;
    }
    let mut stack = vec![(resolved_root, false)];
    while let Some((dir, in_symlink)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let is_sym = file_type.is_symlink();
            let child_sym = in_symlink || is_sym;
            // Follow symlinks when determining directory vs file, matching os.walk default.
            let is_dir = if is_sym {
                path.is_dir()
            } else {
                file_type.is_dir()
            };
            if is_dir {
                let skip = entry.file_name().to_str().is_some_and(is_skipped_dir);
                if !skip {
                    stack.push((path, child_sym));
                }
            } else if language_of(&path).is_some() && !is_test_file(&path) {
                out.push((path, child_sym));
            }
        }
    }
}

/// Walk `roots`, skipping non-source, test, and ignored directories, returning sorted unique resolved paths.
pub fn iter_source_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut unique = BTreeMap::new();
    let mut raw = Vec::new();
    for root in roots {
        raw.clear();
        walk_root(root, &mut raw);
        for (path, needs_resolve) in raw.drain(..) {
            let resolved = if needs_resolve {
                resolve_path(&path)
            } else {
                path
            };
            let key = to_posix(&resolved);
            unique.entry(key).or_insert(resolved);
        }
    }
    unique.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dryer-discover-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_supported_languages_and_ignores_declarations() {
        assert_eq!(language_of(Path::new("src/app/core.clj")), Some("clojure"));
        assert_eq!(language_of(Path::new("src/app/core.cljd")), Some("clojure"));
        assert_eq!(language_of(Path::new("Widget.java")), Some("java"));
        assert_eq!(language_of(Path::new("board.go")), Some("go"));
        assert_eq!(language_of(Path::new("ui/view.tsx")), Some("typescript"));
        assert_eq!(language_of(Path::new("ui/Card.vue")), Some("vue"));
        assert_eq!(language_of(Path::new("src/lib.rs")), Some("rust"));
        assert_eq!(language_of(Path::new("src/dryer/cli.py")), Some("python"));
        assert_eq!(language_of(Path::new("App.swift")), Some("swift"));
        assert_eq!(language_of(Path::new("types.d.ts")), None);
        assert_eq!(language_of(Path::new("notes.md")), None);
        assert_eq!(language_of(Path::new(".leading")), None);
    }

    #[test]
    fn recognizes_test_files_and_dirs() {
        for name in [
            "pkg/foo_test.go",
            "src/app_test.clj",
            "ui/a.test.ts",
            "ui/a.spec.cts",
            "ui/Card.test.vue",
            "tests/conftest.py",
            "pkg/test_foo.py",
            "tests/app.py",
            "Tests/AppTests.swift",
            "Sources/HiddenTests.swift",
        ] {
            assert!(is_test_file(Path::new(name)), "expected test file: {name}");
        }
        assert!(!is_test_file(Path::new("src/app.py")));
    }

    #[test]
    fn walks_and_filters_test_and_skipped_dirs() {
        let dir = temp_dir("walk");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("src/app.py"), "x = 1\n").unwrap();
        fs::write(dir.join("src/foo_test.py"), "x = 1\n").unwrap();
        fs::write(dir.join("target/app.py"), "x = 1\n").unwrap();
        fs::write(dir.join("tests/app.py"), "x = 1\n").unwrap();
        fs::write(dir.join("src/widget.cts"), "export {}\n").unwrap();
        fs::write(dir.join("src/app.test.cts"), "export {}\n").unwrap();

        let root_resolved = resolve_path(&dir);
        let rels: Vec<String> = iter_source_files(std::slice::from_ref(&dir))
            .into_iter()
            .map(|p| to_posix(p.strip_prefix(&root_resolved).unwrap()))
            .collect();
        assert_eq!(rels, vec!["src/app.py", "src/widget.cts"]);
        let _ = fs::remove_dir_all(dir);
    }
}
