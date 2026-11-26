//! Pure function-level identity, privacy, digest, mutation-id, and crapper-backed span helpers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use crapper_lib::model::Function;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::engine::FormSpanMap;
use crate::model::Site;
use crate::path_util::{resolve_path, to_posix};
use crate::sites::discover_raw;

static TS_PRIVATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bprivate\b").expect("valid regex"));
static SWIFT_PRIVATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:fileprivate|private)\b").expect("valid regex"));
static RUST_PUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^pub\s").expect("valid regex"));

fn split_lines_python(source: &str) -> Vec<&str> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&source[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&source[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        lines.push(&source[start..]);
    }
    lines
}

fn slice_lines(source: &str, start: usize, end: usize) -> String {
    let lines = split_lines_python(source);
    let from = start.saturating_sub(1).min(lines.len());
    let to = end.min(lines.len());
    if from >= to {
        return String::new();
    }
    lines[from..to].join("\n")
}

/// SHA-256 hex digest of lines `start..=end` with trailing whitespace stripped per line.
pub fn digest(source: &str, start: usize, end: usize) -> String {
    let lines = split_lines_python(source);
    let from = start.saturating_sub(1).min(lines.len());
    let to = end.min(lines.len());
    let chunk = if from >= to {
        String::new()
    } else {
        lines[from..to]
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut hasher = Sha256::new();
    hasher.update(chunk.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn header(source: &str, start: usize, end: usize) -> String {
    let raw = slice_lines(source, start, end.min(start + 6));
    let mut text = raw.trim_start();
    while text.starts_with("#[") {
        let Some(close) = text.find(']') else {
            break;
        };
        text = text[close + 1..].trim_start();
    }
    text.to_string()
}

fn python_private(name: &str) -> bool {
    if name.starts_with("__") && name.ends_with("__") {
        return false;
    }
    name.starts_with('_')
}

/// True when uml-viewer should treat the operation as private (`defn-/`).
pub fn is_private(language: &str, name: &str, source: &str, start: usize, end: usize) -> bool {
    let hdr = header(source, start, end);
    match language {
        "clojure" => hdr.starts_with("(defn-"),
        "java" => TS_PRIVATE_RE.is_match(&hdr),
        "go" => name.chars().next().is_some_and(|c| c.is_lowercase()),
        "python" => python_private(name),
        "rust" => !RUST_PUB_RE.is_match(&hdr),
        "typescript" => name.starts_with('#') || TS_PRIVATE_RE.is_match(&hdr),
        "swift" => SWIFT_PRIVATE_RE.is_match(&hdr),
        _ => false,
    }
}

/// Form id for uml-viewer (`defn/<name>` or `defn-/<name>`).
pub fn form_id(name: &str, private: bool) -> String {
    let prefix = if private { "defn-" } else { "defn" };
    format!("{prefix}/{name}")
}

/// Stable JSON array key identifying one mutation site.
pub fn mutation_id(
    file: &str,
    namespace: &str,
    form: &str,
    start: usize,
    end: usize,
    original: &str,
    mutant: &str,
) -> String {
    serde_json::to_string(&(file, namespace, form, start, end, original, mutant))
        .expect("tuple serialization cannot fail")
}

fn mutation_field(mutation: &str, index: usize) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(mutation).ok()?;
    let arr = value.as_array()?;
    arr.get(index)?.as_str().map(str::to_string)
}

/// Extract source file key from a serialized `mutation_id`.
pub fn mutation_file(mutation: &str) -> Option<String> {
    mutation_field(mutation, 0)
}

/// Extract namespace from a serialized `mutation_id`.
pub fn mutation_namespace(mutation: &str) -> Option<String> {
    mutation_field(mutation, 1)
}

/// Extract functions reported by `crapper-lib` for `path` under `root`.
pub fn project_functions(source: &str, path: &Path, root: &Path) -> Vec<Function> {
    let Some(language) = crapper_lib::discover::language_of(path) else {
        return Vec::new();
    };
    let resolved_path = to_posix(&resolve_path(path));
    let resolved_root = to_posix(&resolve_path(root));
    crapper_lib::languages::functions_in_file(
        language,
        source,
        &resolved_path,
        Some(resolved_root.as_str()),
    )
}

fn contains(func: &Function, line: usize, byte: Option<usize>) -> bool {
    if func.start_line > line || func.end_line < line {
        return false;
    }
    match (byte, func.start_byte, func.end_byte) {
        (Some(b), Some(start), Some(end)) => start <= b && b < end,
        _ => true,
    }
}

fn tightness(func: &Function) -> usize {
    if let (Some(start), Some(end)) = (func.start_byte, func.end_byte)
        && end >= start
    {
        return end - start;
    }
    func.end_line.saturating_sub(func.start_line) * 1_000_000
}

fn owner(functions: &[Function], line: usize, byte: Option<usize>) -> Option<(usize, &Function)> {
    functions
        .iter()
        .enumerate()
        .filter(|(_, f)| contains(f, line, byte))
        .min_by_key(|(_, f)| (tightness(f), f.start_line))
}

/// Discover mutation sites bound to their enclosing `crapper` function and namespace.
pub fn sites_in_file(source: &str, path: &Path, root: &Path, file_key: &str) -> Vec<Site> {
    let Some(language) = crapper_lib::discover::language_of(path) else {
        return Vec::new();
    };
    let functions = project_functions(source, path, root);
    let privacy: Vec<bool> = functions
        .iter()
        .map(|f| is_private(language, &f.name, source, f.start_line, f.end_line))
        .collect();
    let Ok(raw_sites) = discover_raw(source, language, &to_posix(path)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for raw in raw_sites {
        let Some((idx, owner_fn)) = owner(&functions, raw.line, Some(raw.start)) else {
            continue;
        };
        let form = form_id(&owner_fn.name, privacy[idx]);
        let mid = mutation_id(
            file_key,
            &owner_fn.namespace,
            &form,
            raw.start,
            raw.end,
            &raw.original,
            &raw.mutant,
        );
        found.push(Site {
            file: file_key.to_string(),
            namespace: owner_fn.namespace.clone(),
            form_id: form,
            name: owner_fn.name.clone(),
            line: raw.line,
            start: raw.start,
            end: raw.end,
            original: raw.original,
            mutant: raw.mutant,
            category: raw.category,
            mutation_id: mid,
            anchor_line: raw.anchor_line,
        });
    }
    found
}

/// Map `(namespace, form_id)` to the combined SHA-256 digest of its function bodies.
pub fn form_digests(source: &str, path: &Path, root: &Path) -> HashMap<(String, String), String> {
    let Some(language) = crapper_lib::discover::language_of(path) else {
        return HashMap::new();
    };
    let mut grouped: HashMap<(String, String), Vec<Function>> = HashMap::new();
    for func in project_functions(source, path, root) {
        let private = is_private(language, &func.name, source, func.start_line, func.end_line);
        let key = (func.namespace.clone(), form_id(&func.name, private));
        grouped.entry(key).or_default().push(func);
    }
    let mut digests = HashMap::with_capacity(grouped.len());
    for (key, mut funcs) in grouped {
        funcs.sort_by_key(|f| (f.start_line, f.end_line));
        let combined: String = funcs
            .iter()
            .map(|f| digest(source, f.start_line, f.end_line))
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(combined.as_bytes());
        digests.insert(key, format!("{:x}", hasher.finalize()));
    }
    digests
}

/// Map `(namespace, form_id)` to `(start_line, end_line, name, private)`.
pub fn form_spans(source: &str, path: &Path, root: &Path) -> FormSpanMap {
    let language = crapper_lib::discover::language_of(path).unwrap_or("");
    let mut spans: FormSpanMap = HashMap::new();
    for func in project_functions(source, path, root) {
        let private = is_private(language, &func.name, source, func.start_line, func.end_line);
        let key = (func.namespace.clone(), form_id(&func.name, private));
        spans
            .entry(key)
            .and_modify(|(start, end, _, _)| {
                *start = (*start).min(func.start_line);
                *end = (*end).max(func.end_line);
            })
            .or_insert((func.start_line, func.end_line, func.name, private));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn privacy_headers_and_digests_match() {
        assert!(!is_private(
            "python",
            "__init__",
            "def __init__(self):\n    pass\n",
            1,
            2
        ));
        assert!(is_private(
            "python",
            "_hide",
            "def _hide():\n    return 1\n",
            1,
            2
        ));
        assert!(is_private(
            "python",
            "__x",
            "def __x():\n    return 1\n",
            1,
            2
        ));
        assert!(is_private("rust", "place", "fn place() {}\n", 1, 1));
        assert!(!is_private(
            "rust",
            "place",
            "#[inline]\npub fn place() {}\n",
            1,
            2
        ));
        assert!(is_private("typescript", "#place", "#place() {}\n", 1, 1));
        assert!(is_private(
            "typescript",
            "place",
            "private place() {}\n",
            1,
            1
        ));
        assert!(!is_private(
            "typescript",
            "place",
            "function place() {}\n",
            1,
            1
        ));
        assert!(!is_private("cobol", "place", "PLACE.\n", 1, 1));

        let source = "def place(x):\n    return x > 0\n";
        let expected_hash = {
            let mut h = Sha256::new();
            h.update(b"def place(x):\n    return x > 0");
            format!("{:x}", h.finalize())
        };
        assert_eq!(digest(source, 1, 2), expected_hash);
        assert_eq!(mutation_field("not-json", 0), None);
        assert_eq!(mutation_field("\"hello\"", 0), None);
        assert_eq!(mutation_field("[1]", 0), None);
        assert_eq!(mutation_field("[\"only\"]", 1), None);
        assert_eq!(
            mutation_file("[\"src/a.py\",\"demo\",\"defn/place\",1,2,\">\",\">=\"]").as_deref(),
            Some("src/a.py")
        );
    }

    #[test]
    fn sites_in_file_joins_crapper_namespaces_across_languages() {
        let root = std::env::temp_dir().join(format!("mutator_funcs_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src").join("demo")).unwrap();
        std::fs::create_dir_all(root.join("Sources").join("Demo")).unwrap();

        let py_src = "class Board:\n    def place(self, x):\n        return x > 0\n    def _hide(self):\n        return False\n";
        let py_path = root.join("src").join("demo").join("board.py");
        std::fs::write(&py_path, py_src).unwrap();
        let py_sites = sites_in_file(py_src, &py_path, &root, "src/demo/board.py");
        assert_eq!(
            py_sites
                .iter()
                .map(|s| s.namespace.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["demo.board.Board"])
        );
        assert!(
            py_sites
                .iter()
                .any(|s| s.form_id == "defn/place" && s.original == ">")
        );
        assert!(
            py_sites
                .iter()
                .any(|s| s.form_id == "defn-/_hide" && s.original == "False")
        );

        let js_src = "export function mount(app) {\n  app.get(\"/users\", (req, res) => {\n    return req.body ?? false;\n  });\n}\n";
        let js_path = root.join("src").join("demo").join("routes.js");
        std::fs::write(&js_path, js_src).unwrap();
        let js_sites = sites_in_file(js_src, &js_path, &root, "src/demo/routes.js");
        assert!(
            js_sites
                .iter()
                .any(|s| s.name == "GET /users" && s.original == "??" && s.mutant == "||")
        );
        assert!(js_sites.iter().all(|s| s.name != "mount"));

        let swift_src = "struct Box {\n    private func hide() -> Bool { false }\n    func open() -> Int { 1 }\n}\n";
        let swift_path = root.join("Sources").join("Demo").join("Box.swift");
        std::fs::write(&swift_path, swift_src).unwrap();
        let swift_sites = sites_in_file(swift_src, &swift_path, &root, "Sources/Demo/Box.swift");
        assert!(
            swift_sites
                .iter()
                .any(|s| s.namespace == "Demo.Box" && s.form_id == "defn-/hide")
        );
        assert!(
            swift_sites
                .iter()
                .any(|s| s.namespace == "Demo.Box" && s.form_id == "defn/open")
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
