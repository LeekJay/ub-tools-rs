//! Read and write `.metrics/mutate` snapshots.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::SecondsFormat;

use crate::edn::{EdnValue, dumps, keyword, loads};
use crate::functions::{mutation_file, mutation_namespace};
use crate::model::{FormResult, History, PriorForm};
use crate::path_util::{relative_posix, resolve_path, to_posix};

/// Relative POSIX path of `path` under `root`.
pub fn source_key(path: &Path, root: &Path) -> Result<String, String> {
    relative_posix(path, root)
        .ok_or_else(|| format!("{} is not under {}", path.display(), root.display()))
}

pub(crate) fn source_key_resolved(
    resolved_path: &Path,
    resolved_root: &Path,
) -> Result<String, String> {
    resolved_path
        .strip_prefix(resolved_root)
        .map(to_posix)
        .map_err(|_| {
            format!(
                "{} is not under {}",
                resolved_path.display(),
                resolved_root.display()
            )
        })
}

/// Snapshot `.edn` file path for `namespace` under `root/.metrics/mutate`.
pub fn snapshot_path(root: &Path, namespace: &str) -> Result<PathBuf, String> {
    let normalized = namespace.replace('\\', "/").replace("::", "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    for &part in &parts {
        if matches!(part, "" | "." | "..") {
            return Err(format!("unsafe namespace {namespace:?}"));
        }
    }
    let mut path = root.join(".metrics").join("mutate");
    for part in &parts[..parts.len() - 1] {
        path.push(part);
    }
    path.push(format!("{}.edn", parts[parts.len() - 1]));
    Ok(path)
}

fn collect_edn_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_edn_files(&path, out);
        } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("edn") {
            out.push(path);
        }
    }
}

fn edn_files(root: &Path) -> Vec<PathBuf> {
    let directory = root.join(".metrics").join("mutate");
    if !directory.is_dir() {
        return Vec::new();
    }
    let mut found = Vec::new();
    collect_edn_files(&directory, &mut found);
    found.sort();
    found
}

fn read_edn_map(path: &Path) -> Option<EdnValue> {
    let text = std::fs::read_to_string(path).ok()?;
    let data = loads(&text).ok()?;
    if data.as_map().is_some() {
        Some(data)
    } else {
        None
    }
}

fn forms_of(data: &EdnValue) -> Vec<&EdnValue> {
    let Some(items) = data.get("forms").and_then(EdnValue::as_vec) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.as_map().is_some())
        .collect()
}

fn outcomes_of(data: &EdnValue) -> HashMap<String, String> {
    let Some(entries) = data.get("outcomes").and_then(EdnValue::as_map) else {
        return HashMap::new();
    };
    let mut found = HashMap::new();
    for (k, v) in entries {
        let Some(key) = k.as_str() else {
            continue;
        };
        let Some(val) = v.as_str() else {
            continue;
        };
        let status = val.strip_prefix(':').unwrap_or(val);
        if matches!(status, "killed" | "survived") {
            found.insert(key.to_string(), status.to_string());
        }
    }
    found
}

fn form_file(form: &EdnValue, data: &EdnValue) -> Option<String> {
    if let Some(name) = form.get("file").and_then(EdnValue::as_str)
        && !name.is_empty()
    {
        return Some(name.to_string());
    }
    data.get("source")
        .and_then(EdnValue::as_str)
        .map(str::to_string)
}

fn remember_form(
    history: &mut History,
    namespace: &str,
    form: &EdnValue,
    data: &EdnValue,
    file_key: &str,
) {
    if form_file(form, data).as_deref() != Some(file_key) {
        return;
    }
    let Some(form_id) = form.get("id").and_then(EdnValue::as_str) else {
        return;
    };
    let Some(digest) = form.get("hash").and_then(EdnValue::as_str) else {
        return;
    };
    history.forms.insert(
        (namespace.to_string(), form_id.to_string()),
        PriorForm {
            namespace: namespace.to_string(),
            id: form_id.to_string(),
            digest: digest.to_string(),
            file: file_key.to_string(),
        },
    );
}

fn remember_outcomes(history: &mut History, data: &EdnValue, file_key: &str) {
    for (mutation, status) in outcomes_of(data) {
        if mutation_file(&mutation).as_deref() == Some(file_key) {
            history.outcomes.insert(mutation, status);
        }
    }
}

/// Prior form hashes and outcomes recorded for `file_key`.
pub fn load_history(root: &Path, file_key: &str) -> History {
    let _lock = ub_runtime::acquire_repo_shared_lock("mutate-metrics", root).ok();
    let mut history = History::default();
    for path in edn_files(root) {
        let Some(data) = read_edn_map(&path) else {
            continue;
        };
        let Some(namespace) = data.get("namespace").and_then(EdnValue::as_str) else {
            continue;
        };
        for form in forms_of(&data) {
            remember_form(&mut history, namespace, form, &data, file_key);
        }
        remember_outcomes(&mut history, &data, file_key);
    }
    history
}

/// Pre-parsed snapshot history across all files under `root/.metrics/mutate`.
#[derive(Debug, Default)]
pub struct HistoryIndex {
    by_file: HashMap<String, History>,
}

impl HistoryIndex {
    /// Load `.metrics/mutate/*.edn` once and bucket forms and outcomes by source `file_key`.
    pub fn load(root: &Path) -> Self {
        let _lock = ub_runtime::acquire_repo_shared_lock("mutate-metrics", root).ok();
        let mut by_file: HashMap<String, History> = HashMap::new();
        for path in edn_files(root) {
            let Some(data) = read_edn_map(&path) else {
                continue;
            };
            let Some(namespace) = data.get("namespace").and_then(EdnValue::as_str) else {
                continue;
            };
            for form in forms_of(&data) {
                let Some(file_key) = form_file(form, &data) else {
                    continue;
                };
                let history = by_file.entry(file_key.clone()).or_default();
                remember_form(history, namespace, form, &data, &file_key);
            }
            for (mutation, status) in outcomes_of(&data) {
                let Some(file_key) = mutation_file(&mutation) else {
                    continue;
                };
                by_file
                    .entry(file_key)
                    .or_default()
                    .outcomes
                    .insert(mutation, status);
            }
        }
        Self { by_file }
    }

    /// Prior history for `file_key`, if recorded.
    pub fn get(&self, file_key: &str) -> Option<&History> {
        self.by_file.get(file_key)
    }
}

fn render_form(form: &FormResult) -> EdnValue {
    let kind = if form.private { "defn-" } else { "defn" };
    EdnValue::Map(vec![
        (EdnValue::Str("id".into()), EdnValue::Str(form.id.clone())),
        (EdnValue::Str("kind".into()), EdnValue::Str(kind.into())),
        (
            EdnValue::Str("file".into()),
            EdnValue::Str(form.file.clone()),
        ),
        (
            EdnValue::Str("line".into()),
            EdnValue::Int(form.line as i64),
        ),
        (
            EdnValue::Str("end-line".into()),
            EdnValue::Int(form.end_line as i64),
        ),
        (
            EdnValue::Str("hash".into()),
            EdnValue::Str(form.digest.clone()),
        ),
        (
            EdnValue::Str("killed".into()),
            EdnValue::Int(form.killed as i64),
        ),
        (
            EdnValue::Str("survived".into()),
            EdnValue::Int(form.survived as i64),
        ),
        (
            EdnValue::Str("uncovered".into()),
            EdnValue::Int(form.uncovered as i64),
        ),
        (
            EdnValue::Str("sites".into()),
            EdnValue::Int(form.sites as i64),
        ),
    ])
}

fn render_snapshot(
    namespace: &str,
    source: &str,
    forms: Vec<EdnValue>,
    outcomes: BTreeMap<String, String>,
) -> String {
    let tested_at = chrono::Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);
    let outcome_entries = outcomes
        .into_iter()
        .map(|(k, v)| (EdnValue::Str(k), keyword(v)))
        .collect();
    let payload = EdnValue::Map(vec![
        (EdnValue::Str("version".into()), EdnValue::Int(1)),
        (EdnValue::Str("tested-at".into()), EdnValue::Str(tested_at)),
        (EdnValue::Str("source".into()), EdnValue::Str(source.into())),
        (
            EdnValue::Str("namespace".into()),
            EdnValue::Str(namespace.into()),
        ),
        (
            EdnValue::Str("outcomes".into()),
            EdnValue::Map(outcome_entries),
        ),
        (EdnValue::Str("forms".into()), EdnValue::Vec(forms)),
    ]);
    format!("{}\n", dumps(&payload))
}

fn kept_forms(data: &EdnValue, file_key: &str) -> Vec<EdnValue> {
    forms_of(data)
        .into_iter()
        .filter(|form| form_file(form, data).as_deref() != Some(file_key))
        .cloned()
        .collect()
}

fn kept_outcomes(data: &EdnValue, file_key: &str) -> HashMap<String, String> {
    outcomes_of(data)
        .into_iter()
        .filter(|(mutation, _)| mutation_file(mutation).as_deref() != Some(file_key))
        .collect()
}

fn file_outcomes(outcomes: &HashMap<String, String>, file_key: &str) -> HashMap<String, String> {
    outcomes
        .iter()
        .filter(|(mutation, status)| {
            mutation_file(mutation).as_deref() == Some(file_key)
                && matches!(status.as_str(), "killed" | "survived")
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn read_snapshots(root: &Path) -> Vec<(PathBuf, EdnValue)> {
    edn_files(root)
        .into_iter()
        .filter_map(|path| read_edn_map(&path).map(|data| (path, data)))
        .collect()
}

fn mentions_file(data: &EdnValue, file_key: &str) -> bool {
    forms_of(data)
        .into_iter()
        .any(|form| form_file(form, data).as_deref() == Some(file_key))
        || outcomes_of(data)
            .keys()
            .any(|mutation| mutation_file(mutation).as_deref() == Some(file_key))
}

type RetainedMap = BTreeMap<String, (Vec<EdnValue>, HashMap<String, String>)>;

fn retain(existing: &[(PathBuf, EdnValue)], file_key: &str) -> (RetainedMap, HashSet<String>) {
    let mut retained: RetainedMap = BTreeMap::new();
    let mut previous = HashSet::new();
    for (_path, data) in existing {
        let Some(namespace) = data.get("namespace").and_then(EdnValue::as_str) else {
            continue;
        };
        if mentions_file(data, file_key) {
            previous.insert(namespace.to_string());
        }
        let kf = kept_forms(data, file_key);
        let ko = kept_outcomes(data, file_key);
        if kf.is_empty() && ko.is_empty() {
            continue;
        }
        let entry = retained.entry(namespace.to_string()).or_default();
        entry.0.extend(kf);
        entry.1.extend(ko);
    }
    (retained, previous)
}

fn outcomes_for(outcomes: &HashMap<String, String>, namespace: &str) -> HashMap<String, String> {
    outcomes
        .iter()
        .filter(|(mutation, _)| mutation_namespace(mutation).as_deref() == Some(namespace))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn form_order_key(form: &EdnValue) -> (String, i64, String) {
    let file = form
        .get("file")
        .and_then(EdnValue::as_str)
        .unwrap_or("None")
        .to_string();
    let line = form.get("line").and_then(EdnValue::as_i64).unwrap_or(0);
    let id = form
        .get("id")
        .and_then(EdnValue::as_str)
        .unwrap_or("None")
        .to_string();
    (file, line, id)
}

fn dedupe(forms: Vec<EdnValue>) -> Vec<EdnValue> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(forms.len());
    for form in forms {
        let key = (
            form.get("file")
                .and_then(EdnValue::as_str)
                .map(str::to_string),
            form.get("id")
                .and_then(EdnValue::as_str)
                .map(str::to_string),
            form.get("line").and_then(EdnValue::as_i64),
            form.get("hash")
                .and_then(EdnValue::as_str)
                .map(str::to_string),
        );
        if seen.insert(key) {
            unique.push(form);
        }
    }
    unique
}

fn snapshot_source(fresh: &[FormResult], kept_forms: &[EdnValue], file_key: &str) -> String {
    if let Some(first) = fresh.first() {
        return first.file.clone();
    }
    kept_forms
        .first()
        .and_then(|f| f.get("file").and_then(EdnValue::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or(file_key)
        .to_string()
}

fn write_namespace(
    root: &Path,
    namespace: &str,
    kept_forms: Vec<EdnValue>,
    kept_outcomes: HashMap<String, String>,
    fresh: &[FormResult],
    outcomes: &HashMap<String, String>,
    file_key: &str,
) -> Result<Option<PathBuf>, String> {
    let mut combined = kept_forms.clone();
    combined.extend(fresh.iter().map(render_form));
    let mut rendered = dedupe(combined);
    rendered.sort_by_key(form_order_key);

    let mut merged: BTreeMap<String, String> = kept_outcomes.into_iter().collect();
    if !fresh.is_empty() {
        merged.extend(outcomes_for(outcomes, namespace));
    }

    let path = snapshot_path(root, namespace)?;
    if rendered.is_empty() && merged.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let source = snapshot_source(fresh, &kept_forms, file_key);
    let content = render_snapshot(namespace, &source, rendered, merged);
    ub_runtime::atomic_write(&path, content.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

fn form_file_only(data: &EdnValue, file_key: &str) -> bool {
    let forms = forms_of(data);
    if forms.is_empty() {
        return data.get("source").and_then(EdnValue::as_str) == Some(file_key);
    }
    let files: HashSet<Option<String>> = forms.into_iter().map(|f| form_file(f, data)).collect();
    files.len() == 1 && files.contains(&Some(file_key.to_string()))
}

fn stale(
    path: &Path,
    data: &EdnValue,
    touched: &BTreeSet<String>,
    canonical: &HashSet<PathBuf>,
    file_key: &str,
) -> bool {
    if canonical.contains(&resolve_path(path)) {
        return false;
    }
    if let Some(namespace) = data.get("namespace").and_then(EdnValue::as_str)
        && touched.contains(namespace)
    {
        return true;
    }
    form_file_only(data, file_key)
}

fn drop_stale(
    existing: &[(PathBuf, EdnValue)],
    touched: &BTreeSet<String>,
    canonical: &HashSet<PathBuf>,
    file_key: &str,
) {
    for (path, data) in existing {
        if stale(path, data, touched, canonical, file_key) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn remove_empty_dirs(directory: &Path) {
    if !directory.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            remove_empty_dirs(&p);
            subdirs.push(p);
        }
    }
    for dir in subdirs {
        if std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

/// Replace `file_key`'s forms in `.metrics/mutate/*.edn`, preserving other files sharing a namespace.
pub fn write_results(
    root: &Path,
    file_key: &str,
    forms: &[FormResult],
    outcomes: &HashMap<String, String>,
) -> Result<Vec<PathBuf>, String> {
    let root = resolve_path(root);
    let mut incoming: HashMap<String, Vec<FormResult>> = HashMap::new();
    for form in forms {
        incoming
            .entry(form.namespace.clone())
            .or_default()
            .push(form.clone());
    }
    let incoming_outcomes = file_outcomes(outcomes, file_key);
    let _lock =
        ub_runtime::acquire_repo_lock("mutate-metrics", &root, None).map_err(|e| e.to_string())?;
    let existing = read_snapshots(&root);
    let (mut retained, previous) = retain(&existing, file_key);

    let mut touched: BTreeSet<String> = BTreeSet::new();
    touched.extend(incoming.keys().cloned());
    touched.extend(previous);

    let mut written = Vec::new();
    let mut canonical = HashSet::new();
    for namespace in &touched {
        canonical.insert(resolve_path(&snapshot_path(&root, namespace)?));
        let (kept_forms, kept_outcomes) = retained.remove(namespace).unwrap_or_default();
        let fresh = incoming.get(namespace).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(path) = write_namespace(
            &root,
            namespace,
            kept_forms,
            kept_outcomes,
            fresh,
            &incoming_outcomes,
            file_key,
        )? {
            written.push(path);
        }
    }

    drop_stale(&existing, &touched, &canonical, file_key);
    remove_empty_dirs(&root.join(".metrics").join("mutate"));
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mutator_metrics_test_{name}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_form(
        ns: &str,
        name: &str,
        file: &str,
        digest: &str,
        killed: usize,
        private: bool,
    ) -> FormResult {
        FormResult {
            id: if private {
                format!("defn-/{name}")
            } else {
                format!("defn/{name}")
            },
            namespace: ns.into(),
            name: name.into(),
            private,
            file: file.into(),
            line: 2,
            end_line: 4,
            digest: digest.into(),
            killed,
            survived: 0,
            uncovered: 1,
            sites: 2,
        }
    }

    #[test]
    fn snapshot_paths_and_round_trip() {
        let root = temp_dir("snap");
        assert_eq!(
            snapshot_path(&root, "example.com/demo.Widget").unwrap(),
            root.join(".metrics")
                .join("mutate")
                .join("example.com")
                .join("demo.Widget.edn")
        );
        assert_eq!(
            snapshot_path(&root, "crate::foo::Bar").unwrap(),
            root.join(".metrics")
                .join("mutate")
                .join("crate")
                .join("foo")
                .join("Bar.edn")
        );

        write_results(
            &root,
            "a.go",
            &[make_form("demo", "Run", "a.go", "hash-a", 1, false)],
            &HashMap::new(),
        )
        .unwrap();
        write_results(
            &root,
            "b.go",
            &[make_form("demo", "Stop", "b.go", "hash-b", 4, false)],
            &HashMap::new(),
        )
        .unwrap();

        let path = snapshot_path(&root, "demo").unwrap();
        let data = loads(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            data.get("namespace").and_then(EdnValue::as_str),
            Some("demo")
        );
        assert_eq!(
            data.get("forms").and_then(EdnValue::as_vec).unwrap().len(),
            2
        );

        write_results(
            &root,
            "a.go",
            &[make_form("demo", "Run", "a.go", "hash-a", 9, false)],
            &HashMap::new(),
        )
        .unwrap();
        let again = loads(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let forms = again.get("forms").and_then(EdnValue::as_vec).unwrap();
        let run_form = forms
            .iter()
            .find(|f| f.get("id").and_then(EdnValue::as_str) == Some("defn/Run"))
            .unwrap();
        let stop_form = forms
            .iter()
            .find(|f| f.get("id").and_then(EdnValue::as_str) == Some("defn/Stop"))
            .unwrap();
        assert_eq!(run_form.get("killed").and_then(EdnValue::as_i64), Some(9));
        assert_eq!(stop_form.get("killed").and_then(EdnValue::as_i64), Some(4));

        write_results(&root, "a.go", &[], &HashMap::new()).unwrap();
        write_results(&root, "b.go", &[], &HashMap::new()).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn history_round_trip_and_legacy_fold() {
        let root = temp_dir("hist");
        let outcome =
            "[\"hide.clj\",\"demo.core\",\"defn-/hide\",8,12,\"false\",\"true\"]".to_string();
        let outcomes = HashMap::from([(outcome.clone(), "killed".to_string())]);
        write_results(
            &root,
            "hide.clj",
            &[make_form(
                "demo.core",
                "hide",
                "hide.clj",
                "digest",
                1,
                true,
            )],
            &outcomes,
        )
        .unwrap();
        let history = load_history(&root, "hide.clj");
        let index = HistoryIndex::load(&root);
        let indexed = index.get("hide.clj").unwrap();
        assert_eq!(indexed.forms.len(), history.forms.len());
        assert_eq!(indexed.outcomes, history.outcomes);
        assert!(index.get("missing.clj").is_none());
        assert_eq!(
            history
                .forms
                .get(&("demo.core".into(), "defn-/hide".into()))
                .map(|p| p.digest.as_str()),
            Some("digest")
        );
        assert_eq!(
            history.outcomes.get(&outcome).map(String::as_str),
            Some("killed")
        );

        let stale = root.join(".metrics").join("mutate").join("legacy.edn");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(
            &stale,
            "{:version 1 :namespace \"demo\" :source \"b.go\" :forms [{:id \"defn/Stop\" :file \"b.go\" :line 3 :hash \"hash-b\" :killed 4 :survived 0 :uncovered 1 :sites 2}] :outcomes {}}\n",
        )
        .unwrap();
        write_results(
            &root,
            "a.go",
            &[make_form("demo", "Run", "a.go", "hash-a", 1, false)],
            &HashMap::new(),
        )
        .unwrap();
        assert!(!stale.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn collect_tmp_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_tmp_files(&path, out);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
            {
                out.push(path);
            }
        }
    }

    #[test]
    fn concurrent_write_results_preserve_both_files_and_leave_no_tmp() {
        let root = temp_dir("concurrent");
        let rounds = 12usize;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let root_a = root.clone();
        let barrier_a = std::sync::Arc::clone(&barrier);
        let thread_a = std::thread::spawn(move || {
            let outcome_key = "[\"a.py\",\"demo\",\"defn/alpha\",2,10,\"+\",\"-\"]".to_string();
            for round in 0..rounds {
                barrier_a.wait();
                let outcomes = HashMap::from([(outcome_key.clone(), "killed".to_string())]);
                write_results(
                    &root_a,
                    "a.py",
                    &[make_form(
                        "demo",
                        "alpha",
                        "a.py",
                        "hash-a",
                        round + 1,
                        false,
                    )],
                    &outcomes,
                )
                .expect("write_results a.py");
            }
        });

        let root_b = root.clone();
        let barrier_b = std::sync::Arc::clone(&barrier);
        let thread_b = std::thread::spawn(move || {
            let outcome_key = "[\"b.py\",\"demo\",\"defn/beta\",2,10,\"-\",\"+\"]".to_string();
            for round in 0..rounds {
                barrier_b.wait();
                let outcomes = HashMap::from([(outcome_key.clone(), "survived".to_string())]);
                write_results(
                    &root_b,
                    "b.py",
                    &[make_form(
                        "demo",
                        "beta",
                        "b.py",
                        "hash-b",
                        round + 1,
                        false,
                    )],
                    &outcomes,
                )
                .expect("write_results b.py");
            }
        });

        thread_a.join().expect("thread_a join");
        thread_b.join().expect("thread_b join");

        let path = snapshot_path(&root, "demo").unwrap();
        let parsed = loads(&std::fs::read_to_string(&path).unwrap()).expect("valid shared EDN");
        let forms = parsed.get("forms").and_then(EdnValue::as_vec).unwrap();
        assert_eq!(forms.len(), 2, "expected both files' forms: {forms:?}");
        let alpha = forms
            .iter()
            .find(|f| f.get("file").and_then(EdnValue::as_str) == Some("a.py"))
            .expect("a.py form preserved");
        let beta = forms
            .iter()
            .find(|f| f.get("file").and_then(EdnValue::as_str) == Some("b.py"))
            .expect("b.py form preserved");
        assert_eq!(
            alpha.get("killed").and_then(EdnValue::as_i64),
            Some(rounds as i64)
        );
        assert_eq!(
            beta.get("killed").and_then(EdnValue::as_i64),
            Some(rounds as i64)
        );

        let outcomes = outcomes_of(&parsed);
        assert_eq!(
            outcomes
                .get("[\"a.py\",\"demo\",\"defn/alpha\",2,10,\"+\",\"-\"]")
                .map(String::as_str),
            Some("killed")
        );
        assert_eq!(
            outcomes
                .get("[\"b.py\",\"demo\",\"defn/beta\",2,10,\"-\",\"+\"]")
                .map(String::as_str),
            Some("survived")
        );

        let mut tmps = Vec::new();
        collect_tmp_files(&root.join(".metrics").join("mutate"), &mut tmps);
        assert!(tmps.is_empty(), "leftover tmp files: {tmps:?}");

        // Also verify concurrent writes to distinct namespaces.
        let barrier2 = std::sync::Arc::new(std::sync::Barrier::new(2));
        let root_c = root.clone();
        let b_c = std::sync::Arc::clone(&barrier2);
        let thread_c = std::thread::spawn(move || {
            b_c.wait();
            let key = "[\"c.py\",\"ns.one\",\"defn/gamma\",2,10,\"+\",\"-\"]".to_string();
            let outcomes = HashMap::from([(key, "killed".to_string())]);
            write_results(
                &root_c,
                "c.py",
                &[make_form("ns.one", "gamma", "c.py", "hash-c", 3, false)],
                &outcomes,
            )
            .expect("write_results c.py");
        });
        let root_d = root.clone();
        let b_d = std::sync::Arc::clone(&barrier2);
        let thread_d = std::thread::spawn(move || {
            b_d.wait();
            let key = "[\"d.py\",\"ns.two\",\"defn/delta\",2,10,\"-\",\"+\"]".to_string();
            let outcomes = HashMap::from([(key, "survived".to_string())]);
            write_results(
                &root_d,
                "d.py",
                &[make_form("ns.two", "delta", "d.py", "hash-d", 5, false)],
                &outcomes,
            )
            .expect("write_results d.py");
        });
        thread_c.join().expect("thread_c join");
        thread_d.join().expect("thread_d join");

        let index = HistoryIndex::load(&root);
        assert!(index.get("c.py").is_some());
        assert!(index.get("d.py").is_some());
        collect_tmp_files(&root.join(".metrics").join("mutate"), &mut tmps);
        assert!(tmps.is_empty(), "leftover tmp files: {tmps:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_namespace_write_cleans_up_temp_files() {
        let root = temp_dir("atomic");
        let outcome = "[\"a.py\",\"pkg.demo\",\"defn/run\",2,10,\"+\",\"-\"]".to_string();
        let outcomes = HashMap::from([(outcome.clone(), "killed".to_string())]);
        let path = write_namespace(
            &root,
            "pkg.demo",
            Vec::new(),
            HashMap::new(),
            &[make_form("pkg.demo", "run", "a.py", "h1", 1, false)],
            &outcomes,
            "a.py",
        )
        .expect("initial write_namespace")
        .expect("path returned");
        assert!(path.is_file());
        let parsed = loads(&std::fs::read_to_string(&path).unwrap()).expect("valid EDN");
        assert_eq!(
            parsed.get("namespace").and_then(EdnValue::as_str),
            Some("pkg.demo")
        );

        write_namespace(
            &root,
            "pkg.demo",
            Vec::new(),
            HashMap::new(),
            &[make_form("pkg.demo", "run", "a.py", "h2", 2, false)],
            &outcomes,
            "a.py",
        )
        .expect("overwrite write_namespace");
        let mut tmps = Vec::new();
        collect_tmp_files(&root.join(".metrics").join("mutate"), &mut tmps);
        assert!(tmps.is_empty(), "leftover tmp files on overwrite: {tmps:?}");

        // Force atomic_write failure by occupying the target path with a non-empty directory.
        let blocked_path = snapshot_path(&root, "blocked").unwrap();
        std::fs::create_dir_all(blocked_path.join("inner")).unwrap();
        let err = write_namespace(
            &root,
            "blocked",
            Vec::new(),
            HashMap::new(),
            &[make_form("blocked", "fail", "b.py", "h3", 1, false)],
            &HashMap::new(),
            "b.py",
        )
        .expect_err("write_namespace over directory must fail");
        assert!(!err.is_empty());

        collect_tmp_files(&root.join(".metrics").join("mutate"), &mut tmps);
        assert!(tmps.is_empty(), "leftover tmp files after error: {tmps:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_results_leaves_unrelated_namespaces_untouched_and_unwritten() {
        let root = temp_dir("unrelated_untouched");
        let unrelated_path = snapshot_path(&root, "unrelated.ns").unwrap();
        std::fs::create_dir_all(unrelated_path.parent().unwrap()).unwrap();
        let unrelated_content = "{:version 1 :tested-at \"2020-01-01T00:00:00+00:00\" :source \"other.py\" :namespace \"unrelated.ns\" :outcomes {} :forms [{:id \"defn/keep\" :kind \"defn\" :file \"other.py\" :line 1 :end-line 3 :hash \"h-keep\" :killed 2 :survived 0 :uncovered 0 :sites 2}]}\n";
        std::fs::write(&unrelated_path, unrelated_content).unwrap();

        let written = write_results(
            &root,
            "a.py",
            &[make_form("demo", "run", "a.py", "h1", 1, false)],
            &HashMap::new(),
        )
        .expect("write demo namespace");

        assert_eq!(written.len(), 1);
        assert!(written[0].ends_with("demo.edn"), "got {written:?}");
        assert_eq!(
            std::fs::read_to_string(&unrelated_path).unwrap(),
            unrelated_content
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
