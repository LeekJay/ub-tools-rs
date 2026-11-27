use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::path_util::{resolve_path, to_posix};

pub const EXTENSIONS: &[(&str, &str)] = &[
    (".clj", "clojure"),
    (".cljc", "clojure"),
    (".cljs", "clojure"),
    (".bb", "clojure"),
    (".java", "java"),
    (".go", "go"),
    (".ts", "typescript"),
    (".tsx", "typescript"),
    (".mts", "typescript"),
    (".cts", "typescript"),
    (".js", "typescript"),
    (".jsx", "typescript"),
    (".mjs", "typescript"),
    (".cjs", "typescript"),
    (".rs", "rust"),
    (".py", "python"),
    (".swift", "swift"),
    (".vue", "vue"),
];

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
    ".build",
    ".swiftpm",
    "DerivedData",
];

pub const TEST_DIRS: &[&str] = &["test", "tests", "spec", "specs", "__tests__"];

pub const GO_TEST_SUFFIX: &str = "_test.go";

pub const CLOJURE_TEST_SUFFIXES: &[&str] = &["_test.clj", "_test.cljc", "_test.cljs", "_test.bb"];

pub const JS_TEST_SUFFIXES: &[&str] = &[
    ".test.ts",
    ".spec.ts",
    ".test.tsx",
    ".spec.tsx",
    ".test.mts",
    ".spec.mts",
    ".test.cts",
    ".spec.cts",
    ".test.js",
    ".spec.js",
    ".test.jsx",
    ".spec.jsx",
    ".test.mjs",
    ".spec.mjs",
    ".test.cjs",
    ".spec.cjs",
];

pub const VUE_TEST_SUFFIXES: &[&str] = &[".test.vue", ".spec.vue"];

pub(crate) fn path_suffix(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some(&name[dot..])
}

pub fn language_of(path: impl AsRef<Path>) -> Option<&'static str> {
    let file_path = path.as_ref();
    let name = file_path.file_name()?.to_str()?;
    if name.ends_with(".d.ts") {
        return None;
    }
    let suffix = path_suffix(file_path)?;
    EXTENSIONS
        .iter()
        .find(|&&(ext, _)| ext == suffix)
        .map(|&(_, lang)| lang)
}

fn swift_test(file_path: &Path) -> bool {
    if path_suffix(file_path) != Some(".swift") {
        return false;
    }
    let Some(name) = file_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with("Tests.swift") {
        return true;
    }
    file_path
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.ends_with("Tests")))
}

fn python_test(name: &str) -> bool {
    if name == "conftest.py" {
        return true;
    }
    name.ends_with("_test.py") || (name.starts_with("test_") && name.ends_with(".py"))
}

pub fn is_test_file(path: impl AsRef<Path>) -> bool {
    let file_path = path.as_ref();
    let Some(name) = file_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with(GO_TEST_SUFFIX)
        || CLOJURE_TEST_SUFFIXES.iter().any(|s| name.ends_with(s))
        || JS_TEST_SUFFIXES.iter().any(|s| name.ends_with(s))
        || VUE_TEST_SUFFIXES.iter().any(|s| name.ends_with(s))
    {
        return true;
    }
    if python_test(name) || swift_test(file_path) {
        return true;
    }
    file_path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| TEST_DIRS.contains(&s))
    })
}

fn is_skipped_dir_name(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || TEST_DIRS.contains(&name)
}

fn walk_root(root: &Path) -> Vec<PathBuf> {
    let resolved_root = resolve_path(root);
    if !resolved_root.exists() {
        return Vec::new();
    }
    if resolved_root.is_file() {
        return vec![resolved_root];
    }
    let mut found = Vec::new();
    let walker = WalkDir::new(&resolved_root)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_dir() {
                let name = entry.file_name().to_str().unwrap_or("");
                return !is_skipped_dir_name(name);
            }
            true
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if language_of(path).is_none() || is_test_file(path) {
            continue;
        }
        found.push(path.to_path_buf());
    }
    found
}

pub fn iter_source_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for root in roots {
        for resolved in walk_root(root) {
            let key = to_posix(&resolved);
            seen.insert((key, resolved));
        }
    }
    seen.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection_and_test_suffixes() {
        assert_eq!(language_of("src/app/core.clj"), Some("clojure"));
        assert_eq!(language_of("src/app/core.cljc"), Some("clojure"));
        assert_eq!(language_of("Widget.java"), Some("java"));
        assert_eq!(language_of("board.go"), Some("go"));
        assert_eq!(language_of("ui/view.tsx"), Some("typescript"));
        assert_eq!(language_of("src/app.js"), Some("typescript"));
        assert_eq!(language_of("src/app.mjs"), Some("typescript"));
        assert_eq!(language_of("src/app.cjs"), Some("typescript"));
        assert_eq!(language_of("src/app.jsx"), Some("typescript"));
        assert_eq!(language_of("src/lib.rs"), Some("rust"));
        assert_eq!(language_of("src/crapper/cli.py"), Some("python"));
        assert_eq!(language_of("Sources/Demo/Box.swift"), Some("swift"));
        assert_eq!(language_of("Card.vue"), Some("vue"));
        assert_eq!(language_of("types.d.ts"), None);
        assert_eq!(language_of("notes.md"), None);
        assert_eq!(language_of(".ts"), None);

        assert!(is_test_file("Tests/DemoTests/BoxTests.swift"));
        assert!(is_test_file("AppTests/Board.swift"));
        assert!(!is_test_file("Sources/Demo/Box.swift"));
        assert!(is_test_file("app.test.cts"));
        assert!(is_test_file("app.spec.cts"));
        assert!(!is_test_file("app.cts"));
        assert!(is_test_file("Card.test.vue"));
        assert!(is_test_file("Card.spec.vue"));
        assert!(!is_test_file("Card.vue"));
        assert!(is_test_file("conftest.py"));
    }
}
