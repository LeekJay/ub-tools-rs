use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::Function;
use crate::path_util::resolve_path;
use crate::runners::nearest_marker;

fn span_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<span[^>]*title="(\d+) out of (\d+) forms covered"[^>]*>\s*(\d+)&nbsp;"#)
            .expect("valid span regex")
    })
}

fn doctype_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<!DOCTYPE[^>]*>").expect("valid DOCTYPE regex"))
}

fn go_module_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*module\s+"?([^"\s]+)"?"#).expect("valid go.mod regex"))
}

/// Strip `<!DOCTYPE ...>` declarations before XML parsing.
pub fn strip_doctype(xml: &str) -> String {
    doctype_re().replace_all(xml, "").into_owned()
}

/// Line and branch hits for one file from LCOV `DA` and `BRDA` records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverage {
    /// Line hits: 1-based line number -> `(covered, total)`.
    pub lines: BTreeMap<usize, (usize, usize)>,
    /// Branch hits: 1-based line number -> `(covered, total)`.
    pub branches: BTreeMap<usize, (usize, usize)>,
}

/// One statement segment from `go test -coverprofile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoSegment {
    pub start_line: usize,
    pub end_line: usize,
    pub statements: usize,
    pub hits: u64,
}

/// Code regions `(start_line, end_line, count)`, or xccov summaries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwiftFile {
    pub regions: Vec<(usize, usize, u64)>,
    pub summaries: Vec<(usize, usize, usize)>,
}

impl SwiftFile {
    /// True when the innermost region containing `line` has a count above zero.
    pub fn line_covered(&self, line: usize) -> bool {
        innermost_region(&self.regions, line)
            .map(|(_, _, count)| count > 0)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JacocoMethod {
    pub missed: usize,
    pub covered: usize,
    pub line: usize,
}

impl JacocoMethod {
    pub fn percent(&self) -> f64 {
        coverage_percent(self.covered, self.missed + self.covered)
    }
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn normalize_path(path: &str) -> String {
    let mut text = percent_decode_utf8(path).replace('\\', "/");
    if let Some(rest) = text.strip_prefix("file:") {
        text = rest.to_string();
    }
    while let Some(rest) = text.strip_prefix("./") {
        text = rest.to_string();
    }
    while text.contains("//") {
        text = text.replace("//", "/");
    }
    text
}

fn segments(path: &str) -> Vec<String> {
    normalize_path(path)
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn suffix_match(path: &str, suffix: &str) -> bool {
    let path_parts = segments(path);
    let suffix_parts = segments(suffix);
    if suffix_parts.is_empty() || suffix_parts.len() > path_parts.len() {
        return false;
    }
    path_parts[path_parts.len() - suffix_parts.len()..] == suffix_parts[..]
}

fn exact_key<'a>(items: &'a [(String, String)], candidates: &[String]) -> Option<&'a str> {
    for candidate in candidates {
        for (original, norm) in items {
            if norm == candidate {
                return Some(original.as_str());
            }
        }
    }
    None
}

fn nonempty_parts(candidates: &[String]) -> Vec<(String, Vec<String>)> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let parts = segments(candidate);
            if parts.is_empty() {
                None
            } else {
                Some((candidate.clone(), parts))
            }
        })
        .collect()
}

fn key_has_suffix(key_parts: &[String], parts: &[String]) -> bool {
    key_parts.len() >= parts.len() && key_parts[key_parts.len() - parts.len()..] == *parts
}

fn longer_source_owns(
    key_parts: &[String],
    source_len: usize,
    source_parts: &[Vec<String>],
) -> bool {
    source_parts.iter().any(|other| {
        other.len() > source_len
            && other.len() <= key_parts.len()
            && key_parts[key_parts.len() - other.len()..] == other[..]
    })
}

fn forward_matches(
    items: &[(String, String)],
    parts_of: &[(String, Vec<String>)],
    source_parts: &[Vec<String>],
) -> Vec<String> {
    let mut forward: Vec<(usize, String)> = Vec::new();
    for (_candidate, parts) in parts_of {
        for (original, norm) in items {
            let key_parts = segments(norm);
            if !key_has_suffix(&key_parts, parts) {
                continue;
            }
            if longer_source_owns(&key_parts, parts.len(), source_parts) {
                continue;
            }
            forward.push((key_parts.len() - parts.len(), original.clone()));
        }
    }
    let Some(smallest) = forward.iter().map(|(extra, _)| *extra).min() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (extra, key) in forward {
        if extra == smallest && !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

fn key_is_proper_suffix(parts: &[String], key_parts: &[String]) -> bool {
    !key_parts.is_empty()
        && key_parts.len() < parts.len()
        && parts[parts.len() - key_parts.len()..] == *key_parts
}

fn another_source_ends_with(
    key_parts: &[String],
    source_parts: &[Vec<String>],
    own: &BTreeSet<Vec<String>>,
) -> bool {
    source_parts.iter().any(|other| {
        !own.contains(other)
            && other.len() >= key_parts.len()
            && other[other.len() - key_parts.len()..] == *key_parts
    })
}

fn reverse_key(
    items: &[(String, String)],
    parts_of: &[(String, Vec<String>)],
    source_parts: &[Vec<String>],
) -> Option<String> {
    let own: BTreeSet<Vec<String>> = parts_of.iter().map(|(_, p)| p.clone()).collect();
    let mut unique: Vec<String> = Vec::new();
    for (_candidate, parts) in parts_of {
        for (original, norm) in items {
            let key_parts = segments(norm);
            if !key_is_proper_suffix(parts, &key_parts) {
                continue;
            }
            if another_source_ends_with(&key_parts, source_parts, &own) {
                continue;
            }
            if !unique.contains(original) {
                unique.push(original.clone());
            }
        }
    }
    if unique.len() == 1 {
        unique.into_iter().next()
    } else {
        None
    }
}

pub fn candidates(source_path: &str) -> Vec<String> {
    let relative = normalize_path(source_path);
    let absolute = if source_path.is_empty() {
        relative.clone()
    } else {
        normalize_path(&resolve_path(Path::new(source_path)).to_string_lossy())
    };
    let no_src = relative
        .strip_prefix("src/")
        .unwrap_or(&relative)
        .to_string();
    let absolute_no_src = absolute.replacen("/src/", "/", 1);
    let mut found = Vec::new();
    for value in [relative, absolute, no_src, absolute_no_src] {
        if !value.is_empty() && !found.contains(&value) {
            found.push(value);
        }
    }
    found
}

/// Select the report key matching `source_path`, preserving longest-suffix priority.
pub fn select_key(
    keys: &[String],
    source_path: &str,
    source_parts: &[Vec<String>],
) -> Option<String> {
    let items: Vec<(String, String)> = keys
        .iter()
        .map(|k| (k.clone(), normalize_path(k)))
        .collect();
    let cands = candidates(source_path);
    if let Some(exact) = exact_key(&items, &cands) {
        return Some(exact.to_string());
    }
    let parts_of = nonempty_parts(&cands);
    let forward = forward_matches(&items, &parts_of, source_parts);
    if forward.len() == 1 {
        return forward.into_iter().next();
    }
    if !forward.is_empty() {
        return None;
    }
    reverse_key(&items, &parts_of, source_parts)
}

/// Lookup a value in any path-keyed map using suffix-aware key resolution.
pub fn lookup<'a, V>(
    index: &'a BTreeMap<String, V>,
    source_path: &str,
    source_parts: &[Vec<String>],
) -> Option<&'a V> {
    if index.is_empty() {
        return None;
    }
    let keys: Vec<String> = index.keys().cloned().collect();
    let key = select_key(&keys, source_path, source_parts)?;
    index.get(&key)
}

pub fn coverage_percent(covered: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    100.0 * (covered as f64) / (total as f64)
}

pub fn percent_for_range(
    lines: Option<&BTreeMap<usize, (usize, usize)>>,
    start: usize,
    end: usize,
) -> Option<f64> {
    let map = lines?;
    let mut covered = 0usize;
    let mut total = 0usize;
    for line in start..=end {
        if let Some(&(c, t)) = map.get(&line) {
            covered += c;
            total += t;
        }
    }
    if total == 0 {
        return Some(0.0);
    }
    Some(coverage_percent(covered, total))
}

pub fn parse_form_coverage(html: &str) -> BTreeMap<usize, (usize, usize)> {
    let mut found = BTreeMap::new();
    for cap in span_re().captures_iter(html) {
        let Ok(covered) = cap[1].parse::<usize>() else {
            continue;
        };
        let Ok(total_forms) = cap[2].parse::<usize>() else {
            continue;
        };
        let Ok(line) = cap[3].parse::<usize>() else {
            continue;
        };
        if total_forms == 0 {
            continue;
        }
        found.insert(line, (covered, total_forms));
    }
    found
}

fn branch_hit(taken: &str) -> bool {
    if taken == "-" {
        return false;
    }
    taken.parse::<i64>().unwrap_or(0) > 0
}

fn add_branch(record: &mut FileCoverage, line: usize, taken: &str) {
    let entry = record.branches.entry(line).or_insert((0, 0));
    if branch_hit(taken) {
        entry.0 += 1;
    }
    entry.1 += 1;
}

fn store_lcov(
    out: &mut BTreeMap<String, FileCoverage>,
    current_file: Option<String>,
    current: FileCoverage,
) {
    if let Some(file) = current_file {
        out.insert(file, current);
    }
}

fn leading_digits(input: &str) -> Option<&str> {
    let end = input.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 { None } else { Some(&input[..end]) }
}

fn read_lcov_line(current: &mut FileCoverage, line: &str) {
    if let Some(rest) = line.strip_prefix("DA:") {
        let mut parts = rest.split(',');
        if let (Some(ln_s), Some(hits_s)) = (parts.next(), parts.next())
            && !ln_s.is_empty()
            && ln_s.bytes().all(|b| b.is_ascii_digit())
            && let Some(hits_digits) = leading_digits(hits_s)
            && let (Ok(ln), Ok(hits)) = (ln_s.parse::<usize>(), hits_digits.parse::<u64>())
        {
            current.lines.insert(ln, (usize::from(hits > 0), 1));
        }
        return;
    }
    if let Some(rest) = line.strip_prefix("BRDA:") {
        let mut parts = rest.split(',');
        let (Some(ln_s), Some(_block), Some(_branch), Some(taken_s)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return;
        };
        if ln_s.is_empty() || !ln_s.bytes().all(|b| b.is_ascii_digit()) {
            return;
        }
        let Ok(ln) = ln_s.parse::<usize>() else {
            return;
        };
        let taken_token = if taken_s.starts_with('-') {
            Some("-")
        } else {
            leading_digits(taken_s)
        };
        if let Some(taken) = taken_token {
            add_branch(current, ln, taken);
        }
    }
}

pub fn parse_lcov(text: &str) -> BTreeMap<String, FileCoverage> {
    let mut out = BTreeMap::new();
    let mut current_file: Option<String> = None;
    let mut current = FileCoverage::default();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(sf) = line.strip_prefix("SF:") {
            store_lcov(&mut out, current_file.take(), current);
            current_file = Some(sf.to_string());
            current = FileCoverage::default();
        } else if line == "end_of_record" {
            store_lcov(&mut out, current_file.take(), current);
            current = FileCoverage::default();
        } else if current_file.is_some() {
            read_lcov_line(&mut current, line);
        }
    }
    store_lcov(&mut out, current_file, current);
    out
}

pub fn branch_percent(record: &FileCoverage, start: usize, end: usize) -> Option<f64> {
    if record.branches.is_empty() {
        return None;
    }
    let mut covered = 0usize;
    let mut total = 0usize;
    for line in start..=end {
        if let Some(&(c, t)) = record.branches.get(&line) {
            covered += c;
            total += t;
        }
    }
    if total == 0 {
        return None;
    }
    Some(coverage_percent(covered, total))
}

pub fn parse_go_profile(text: &str) -> Result<BTreeMap<String, Vec<GoSegment>>, String> {
    let mut out: BTreeMap<String, Vec<GoSegment>> = BTreeMap::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_number = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with("mode:") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3 || !fields[0].contains(':') {
            return Err(format!(
                "invalid coverage segment on line {line_number}: {raw:?}"
            ));
        }
        let (file_part, range_part) = fields[0]
            .split_once(':')
            .ok_or_else(|| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        let (start_text, end_text) = range_part
            .split_once(',')
            .ok_or_else(|| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        let start_line = start_text
            .split('.')
            .next()
            .unwrap_or("")
            .parse::<usize>()
            .map_err(|_| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        let end_line = end_text
            .split('.')
            .next()
            .unwrap_or("")
            .parse::<usize>()
            .map_err(|_| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        let statements = fields[1]
            .parse::<usize>()
            .map_err(|_| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        let hits = fields[2]
            .parse::<u64>()
            .map_err(|_| format!("invalid coverage segment on line {line_number}: {raw:?}"))?;
        out.entry(file_part.to_string())
            .or_default()
            .push(GoSegment {
                start_line,
                end_line,
                statements,
                hits,
            });
    }
    Ok(out)
}

pub fn go_import_name(file: &Path, stop: &Path) -> Option<String> {
    let module_dir = nearest_marker(file, "go.mod", stop)?;
    let mod_text = fs::read_to_string(module_dir.join("go.mod")).ok()?;
    let cap = go_module_re().captures(&mod_text)?;
    let module_name = &cap[1];
    let rel = file.strip_prefix(&module_dir).ok()?;
    let rel_posix = crate::path_util::to_posix(rel);
    Some(format!("{module_name}/{rel_posix}"))
}

fn segments_percent(segments: Option<&[GoSegment]>, start: usize, end: usize) -> Option<f64> {
    let segs = segments?;
    let mut total = 0usize;
    let mut covered = 0usize;
    for seg in segs {
        if seg.end_line < start || seg.start_line > end {
            continue;
        }
        total += seg.statements;
        if seg.hits > 0 {
            covered += seg.statements;
        }
    }
    if total == 0 {
        return Some(0.0);
    }
    Some(coverage_percent(covered, total))
}

pub fn go_percent(
    profile: Option<&BTreeMap<String, Vec<GoSegment>>>,
    path: &str,
    start: usize,
    end: usize,
    source_parts: Option<&[Vec<String>]>,
) -> Option<f64> {
    let map = profile?;
    let parts = source_parts.unwrap_or(&[]);
    let segs = lookup(map, path, parts).map(Vec::as_slice);
    segments_percent(segs, start, end)
}

fn json_usize(v: &Value) -> Option<usize> {
    v.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .or_else(|| v.as_i64().and_then(|n| usize::try_from(n).ok()))
}

fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
}

fn code_region(region: &Value) -> Option<(usize, usize, u64)> {
    let arr = region.as_array()?;
    if arr.len() < 5 {
        return None;
    }
    if arr.len() >= 8 {
        let kind = &arr[7];
        if !kind.is_null() && kind.as_i64() != Some(0) {
            return None;
        }
    }
    Some((
        json_usize(&arr[0])?,
        json_usize(&arr[2])?,
        json_u64(&arr[4])?,
    ))
}

fn segment_counts(arr: &[Value]) -> bool {
    if arr[3].as_bool() != Some(true) {
        return false;
    }
    if arr.len() >= 5 && arr[4].as_bool() != Some(true) {
        return false;
    }
    if arr.len() >= 6 && arr[5].as_bool() == Some(true) {
        return false;
    }
    true
}

fn segment_region(segment: &Value) -> Option<(usize, usize, u64)> {
    let arr = segment.as_array()?;
    if arr.len() < 4 || !segment_counts(arr) {
        return None;
    }
    let line = json_usize(&arr[0])?;
    let count = json_u64(&arr[2])?;
    Some((line, line, count))
}

fn region_file_id(arr: &[Value]) -> usize {
    if arr.len() <= 5 {
        return 0;
    }
    json_usize(&arr[5]).unwrap_or(0)
}

fn filename_at(names: &[Option<String>], index: usize) -> Option<&str> {
    names
        .get(index)
        .and_then(|opt| opt.as_deref())
        .filter(|s| !s.is_empty())
}

fn unit_filenames(unit: &serde_json::Map<String, Value>) -> Vec<Option<String>> {
    unit.get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .map(|file| {
                    file.as_object()
                        .and_then(|obj| obj.get("filename"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn function_filenames(
    function: &serde_json::Map<String, Value>,
    unit_names: &[Option<String>],
) -> Vec<Option<String>> {
    if let Some(arr) = function.get("filenames").and_then(Value::as_array)
        && !arr.is_empty()
    {
        return arr.iter().map(|v| v.as_str().map(str::to_string)).collect();
    }
    unit_names.to_vec()
}

fn llvm_function_regions(
    unit: &serde_json::Map<String, Value>,
) -> BTreeMap<String, Vec<(usize, usize, u64)>> {
    let mut found: BTreeMap<String, Vec<(usize, usize, u64)>> = BTreeMap::new();
    let unit_names = unit_filenames(unit);
    let Some(functions) = unit.get("functions").and_then(Value::as_array) else {
        return found;
    };
    for func in functions {
        let Some(obj) = func.as_object() else {
            continue;
        };
        let names = function_filenames(obj, &unit_names);
        let Some(regions) = obj.get("regions").and_then(Value::as_array) else {
            continue;
        };
        for region in regions {
            let Some(parsed) = code_region(region) else {
                continue;
            };
            let Some(arr) = region.as_array() else {
                continue;
            };
            if let Some(name) = filename_at(&names, region_file_id(arr)) {
                found.entry(name.to_string()).or_default().push(parsed);
            }
        }
    }
    found
}

fn llvm_unit(found: &mut BTreeMap<String, SwiftFile>, unit: &serde_json::Map<String, Value>) {
    let regions = llvm_function_regions(unit);
    let mut seen = BTreeSet::new();
    if let Some(files) = unit.get("files").and_then(Value::as_array) {
        for file in files {
            let Some(obj) = file.as_object() else {
                continue;
            };
            let Some(name) = obj.get("filename").and_then(Value::as_str) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            seen.insert(name.to_string());
            let chosen = if let Some(fn_regs) = regions.get(name) {
                fn_regs.clone()
            } else if let Some(segs) = obj.get("segments").and_then(Value::as_array) {
                segs.iter().filter_map(segment_region).collect()
            } else {
                Vec::new()
            };
            found
                .entry(name.to_string())
                .or_default()
                .regions
                .extend(chosen);
        }
    }
    for (name, parsed) in regions {
        if !seen.contains(&name) {
            found.entry(name).or_default().regions.extend(parsed);
        }
    }
}

fn is_llvm(data: &Value) -> bool {
    let Some(obj) = data.as_object() else {
        return false;
    };
    if obj.get("type").and_then(Value::as_str) == Some("llvm.coverage.json.export") {
        return true;
    }
    obj.get("data").is_some_and(Value::is_array)
}

fn llvm_files(data: &Value) -> BTreeMap<String, SwiftFile> {
    let mut found = BTreeMap::new();
    if let Some(units) = data
        .as_object()
        .and_then(|o| o.get("data"))
        .and_then(Value::as_array)
    {
        for unit in units {
            if let Some(obj) = unit.as_object() {
                llvm_unit(&mut found, obj);
            }
        }
    }
    found
}

fn xccov_summary(func: &Value) -> Option<(usize, usize, usize)> {
    let obj = func.as_object()?;
    let line = json_usize(obj.get("lineNumber")?)?;
    let executable = json_usize(obj.get("executableLines")?)?;
    let covered = obj
        .get("coveredLines")
        .filter(|v| !v.is_null())
        .map(json_usize)
        .unwrap_or(Some(0))?;
    Some((line, covered, executable))
}

fn xccov_file(found: &mut BTreeMap<String, SwiftFile>, file: &serde_json::Map<String, Value>) {
    let path = file
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file.get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    let Some(path) = path else {
        return;
    };
    let record = found.entry(path.to_string()).or_default();
    if let Some(funcs) = file.get("functions").and_then(Value::as_array) {
        for f in funcs {
            if let Some(summary) = xccov_summary(f) {
                record.summaries.push(summary);
            }
        }
    }
}

fn xccov_files(data: &Value) -> BTreeMap<String, SwiftFile> {
    let mut targets: Vec<&serde_json::Map<String, Value>> = Vec::new();
    if let Some(arr) = data.as_array() {
        for item in arr {
            if let Some(obj) = item.as_object() {
                targets.push(obj);
            }
        }
    } else if let Some(obj) = data.as_object() {
        if let Some(arr) = obj.get("targets").and_then(Value::as_array) {
            for item in arr {
                if let Some(t) = item.as_object() {
                    targets.push(t);
                }
            }
        } else if obj.get("files").is_some_and(Value::is_array) {
            targets.push(obj);
        }
    }
    let mut found = BTreeMap::new();
    for target in targets {
        if let Some(files) = target.get("files").and_then(Value::as_array) {
            for file in files {
                if let Some(obj) = file.as_object() {
                    xccov_file(&mut found, obj);
                }
            }
        }
    }
    found
}

pub fn parse_swift_coverage(text: &str) -> Result<BTreeMap<String, SwiftFile>, String> {
    let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if is_llvm(&data) {
        Ok(llvm_files(&data))
    } else {
        Ok(xccov_files(&data))
    }
}

fn innermost_region(regions: &[(usize, usize, u64)], line: usize) -> Option<(usize, usize, u64)> {
    regions
        .iter()
        .copied()
        .filter(|&(start, end, _)| start <= line && line <= end)
        .min_by_key(|&(start, end, _)| (end - start, -(start as isize)))
}

fn region_percent(regions: &[(usize, usize, u64)], start: usize, end: usize) -> f64 {
    let mut total = 0usize;
    let mut covered = 0usize;
    for &(reg_start, _reg_end, count) in regions {
        if reg_start < start || reg_start > end {
            continue;
        }
        total += 1;
        if count > 0 {
            covered += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    coverage_percent(covered, total)
}

fn summary_percent(summaries: &[(usize, usize, usize)], start: usize, end: usize) -> f64 {
    let chosen = summaries
        .iter()
        .copied()
        .filter(|&(line, _, _)| start <= line && line <= end)
        .min_by_key(|&(line, _, _)| line);
    match chosen {
        None => 0.0,
        Some((_, covered, executable)) => coverage_percent(covered, executable),
    }
}

pub fn parse_jacoco_index(text: &str) -> Result<BTreeMap<String, Vec<JacocoMethod>>, String> {
    let cleaned = strip_doctype(text);
    let doc = roxmltree::Document::parse(&cleaned).map_err(|e| e.to_string())?;
    let mut found: BTreeMap<String, Vec<JacocoMethod>> = BTreeMap::new();
    for class_node in doc.descendants().filter(|n| n.has_tag_name("class")) {
        let class_name = class_node.attribute("name").unwrap_or("").replace('/', ".");
        for method in class_node.children().filter(|n| n.has_tag_name("method")) {
            let Some(counter) = method
                .children()
                .find(|c| c.has_tag_name("counter") && c.attribute("type") == Some("INSTRUCTION"))
            else {
                continue;
            };
            let missed = counter
                .attribute("missed")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let covered = counter
                .attribute("covered")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let line = method
                .attribute("line")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let key = format!("{}#{}", class_name, method.attribute("name").unwrap_or(""));
            found.entry(key).or_default().push(JacocoMethod {
                missed,
                covered,
                line,
            });
        }
    }
    Ok(found)
}

/// Parse JaCoCo `<package><sourcefile><line nr ci cb>` into covered line sets per `pkg/File.java`.
/// Used by `mutator` for line-level Java coverage.
pub fn parse_jacoco_source_lines(text: &str) -> Result<BTreeMap<String, BTreeSet<usize>>, String> {
    let cleaned = strip_doctype(text);
    let doc = roxmltree::Document::parse(&cleaned).map_err(|e| e.to_string())?;
    let mut found: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for package in doc.descendants().filter(|n| n.has_tag_name("package")) {
        let pkg = package.attribute("name").unwrap_or("").trim_matches('/');
        for source in package.children().filter(|n| n.has_tag_name("sourcefile")) {
            let filename = source.attribute("name").unwrap_or("");
            let key = if pkg.is_empty() {
                filename.to_string()
            } else {
                format!("{pkg}/{filename}")
            };
            let mut lines = BTreeSet::new();
            for line_node in source.children().filter(|n| n.has_tag_name("line")) {
                let nr = line_node
                    .attribute("nr")
                    .and_then(|s| s.parse::<usize>().ok());
                let ci = line_node
                    .attribute("ci")
                    .and_then(|s| s.parse::<usize>().ok());
                let cb = line_node
                    .attribute("cb")
                    .and_then(|s| s.parse::<usize>().ok());
                if let (Some(n), Some(i), Some(b)) = (nr, ci, cb)
                    && (i > 0 || b > 0)
                {
                    lines.insert(n);
                }
            }
            found.insert(key, lines);
        }
    }
    Ok(found)
}

#[derive(Debug, Clone, Default)]
pub struct CoverageBundle {
    pub lcov: BTreeMap<String, FileCoverage>,
    pub go_profile: Option<BTreeMap<String, Vec<GoSegment>>>,
    pub jacoco: Option<BTreeMap<String, Vec<JacocoMethod>>>,
    pub form_html: BTreeMap<String, BTreeMap<usize, (usize, usize)>>,
    pub swift: BTreeMap<String, SwiftFile>,
    pub source_parts: Vec<Vec<String>>,
    pub root: Option<PathBuf>,
}

impl CoverageBundle {
    pub fn bind_sources(&mut self, paths: &[String]) {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        for source in paths {
            for candidate in candidates(source) {
                let parts = segments(&candidate);
                if !parts.is_empty() && seen.insert(parts.clone()) {
                    found.push(parts);
                }
            }
        }
        self.source_parts = found;
    }

    pub fn percent_for(&self, function: &Function) -> Option<f64> {
        match function.language.as_str() {
            "java" => self.java_percent(function),
            "go" => self.go_file_percent(function),
            "swift" => self.swift_percent(function),
            _ => self.file_percent(function),
        }
    }

    /// Returns true if any loaded coverage report contains records for the given entry.
    pub fn has_coverage_for_entry(
        &self,
        path: &str,
        language: &str,
        namespace: &str,
        name: &str,
    ) -> bool {
        match language {
            "java" => self.jacoco.as_ref().is_some_and(|classes| {
                let exact = format!("{namespace}#{name}");
                let ns_hash = format!("{namespace}#");
                let ns_inner = format!("{namespace}$");
                classes
                    .keys()
                    .any(|k| k == &exact || k.starts_with(&ns_hash) || k.starts_with(&ns_inner))
            }),
            "go" => self.go_profile.as_ref().is_some_and(|profile| {
                if let Some(root) = &self.root {
                    let resolved = resolve_path(&root.join(path));
                    if let Some(import_name) = go_import_name(&resolved, root)
                        && profile.contains_key(&import_name)
                    {
                        return true;
                    }
                }
                lookup(profile, path, &self.source_parts).is_some()
            }),
            "swift" => lookup(&self.swift, path, &self.source_parts).is_some(),
            _ => {
                lookup(&self.form_html, path, &self.source_parts).is_some()
                    || lookup(&self.lcov, path, &self.source_parts).is_some()
            }
        }
    }

    fn java_percent(&self, function: &Function) -> Option<f64> {
        let classes = self.jacoco.as_ref()?;
        let mut names = Vec::new();
        if let Some(jc) = &function.jacoco_class {
            names.push(jc.clone());
        }
        if !names.contains(&function.namespace) {
            names.push(function.namespace.clone());
        }
        let mut pool: Vec<&JacocoMethod> = Vec::new();
        for name in names {
            let key = format!("{name}#{}", function.name);
            if let Some(methods) = classes.get(&key) {
                pool.extend(methods.iter());
            }
        }
        if pool.is_empty() {
            return None;
        }
        if let Some(exact) = pool.iter().find(|m| m.line == function.start_line) {
            return Some(exact.percent());
        }
        let nearest = pool
            .iter()
            .min_by_key(|m| m.line.abs_diff(function.start_line))?;
        Some(nearest.percent())
    }

    fn go_file_percent(&self, function: &Function) -> Option<f64> {
        let profile = self.go_profile.as_ref()?;
        if let Some(root) = &self.root {
            let resolved = resolve_path(&root.join(&function.path));
            if let Some(name) = go_import_name(&resolved, root)
                && let Some(segs) = profile.get(&name)
            {
                return segments_percent(Some(segs), function.start_line, function.end_line);
            }
        }
        let segs = lookup(profile, &function.path, &self.source_parts).map(Vec::as_slice);
        segments_percent(segs, function.start_line, function.end_line)
    }

    fn swift_percent(&self, function: &Function) -> Option<f64> {
        let record = lookup(&self.swift, &function.path, &self.source_parts)?;
        if !record.regions.is_empty() {
            Some(region_percent(
                &record.regions,
                function.start_line,
                function.end_line,
            ))
        } else {
            Some(summary_percent(
                &record.summaries,
                function.start_line,
                function.end_line,
            ))
        }
    }

    fn file_percent(&self, function: &Function) -> Option<f64> {
        if let Some(html) = lookup(&self.form_html, &function.path, &self.source_parts) {
            return percent_for_range(Some(html), function.start_line, function.end_line);
        }
        let record = lookup(&self.lcov, &function.path, &self.source_parts)?;
        if let Some(branched) = branch_percent(record, function.start_line, function.end_line) {
            return Some(branched);
        }
        percent_for_range(Some(&record.lines), function.start_line, function.end_line)
    }
}

fn read_text(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn collect_recursive_named(dir: &Path, filename: &str, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && e.file_name() == "lcov-report"))
        .flatten()
    {
        if entry.file_type().is_file() && entry.file_name().to_string_lossy() == filename {
            out.push(entry.path().to_path_buf());
        }
    }
}

fn is_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| crate::discover::SKIP_DIRS.contains(&name))
}

fn merge_texts(paths: Vec<PathBuf>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut texts = Vec::new();
    for path in paths {
        let resolved = resolve_path(&path);
        if !seen.insert(resolved) {
            continue;
        }
        if let Some(text) = read_text(&path)
            && !text.is_empty()
        {
            texts.push(text);
        }
    }
    texts
}

fn subdirs_1_and_2(root: &Path, tail: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let join_tail = |base: &Path| -> PathBuf {
        let mut p = base.to_path_buf();
        for part in tail {
            p.push(part);
        }
        p
    };
    paths.push(join_tail(root));
    if let Ok(rd1) = fs::read_dir(root) {
        for e1 in rd1.flatten() {
            let p1 = e1.path();
            if !p1.is_dir() || is_skip_dir(&p1) {
                continue;
            }
            paths.push(join_tail(&p1));
            if let Ok(rd2) = fs::read_dir(&p1) {
                for e2 in rd2.flatten() {
                    let p2 = e2.path();
                    if p2.is_dir() && !is_skip_dir(&p2) {
                        paths.push(join_tail(&p2));
                    }
                }
            }
        }
    }
    paths
}

fn sort_paths_by_mtime_asc(paths: &mut [PathBuf]) {
    paths.sort_by_key(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
}

fn merge_lcov(root: &Path) -> BTreeMap<String, FileCoverage> {
    let mut paths = Vec::new();
    collect_recursive_named(
        &root.join("target").join("coverage"),
        "lcov.info",
        &mut paths,
    );
    collect_recursive_named(&root.join("coverage"), "lcov.info", &mut paths);
    sort_paths_by_mtime_asc(&mut paths);
    let mut merged = BTreeMap::new();
    for text in merge_texts(paths) {
        merged.extend(parse_lcov(&text));
    }
    merged
}

fn merge_go(root: &Path) -> BTreeMap<String, Vec<GoSegment>> {
    let mut paths = vec![
        root.join("target").join("coverage").join("coverage.out"),
        root.join("target")
            .join("coverage")
            .join("go")
            .join("coverage.out"),
        root.join("coverage.out"),
    ];
    for candidate in subdirs_1_and_2(root, &["target", "coverage", "go", "coverage.out"]) {
        paths.push(candidate);
    }
    let mut merged: BTreeMap<String, Vec<GoSegment>> = BTreeMap::new();
    for text in merge_texts(paths) {
        if let Ok(parsed) = parse_go_profile(&text) {
            for (key, segs) in parsed {
                merged.entry(key).or_default().extend(segs);
            }
        }
    }
    merged
}

fn merge_jacoco(root: &Path) -> BTreeMap<String, Vec<JacocoMethod>> {
    let paths = subdirs_1_and_2(root, &["target", "site", "jacoco", "jacoco.xml"]);
    let mut merged: BTreeMap<String, Vec<JacocoMethod>> = BTreeMap::new();
    for text in merge_texts(paths) {
        if let Ok(parsed) = parse_jacoco_index(&text) {
            for (key, methods) in parsed {
                merged.entry(key).or_default().extend(methods);
            }
        }
    }
    merged
}

fn merge_forms(root: &Path) -> BTreeMap<String, BTreeMap<usize, (usize, usize)>> {
    let coverage_dir = root.join("target").join("coverage");
    let mut found = BTreeMap::new();
    if !coverage_dir.is_dir() {
        return found;
    }
    for entry in walkdir::WalkDir::new(&coverage_dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let html_path = entry.path();
        if html_path.extension().and_then(|s| s.to_str()) != Some("html") {
            continue;
        }
        let Some(text) = read_text(html_path) else {
            continue;
        };
        if !text.contains("forms covered") {
            continue;
        }
        let Ok(rel) = html_path.strip_prefix(&coverage_dir) else {
            continue;
        };
        let rel_str = crate::path_util::to_posix(rel);
        let key = rel_str
            .strip_suffix(".html")
            .unwrap_or(&rel_str)
            .to_string();
        found.insert(key, parse_form_coverage(&text));
    }
    found
}

fn merge_swift(root: &Path) -> BTreeMap<String, SwiftFile> {
    let coverage = root.join("target").join("coverage").join("swift");
    if !coverage.is_dir() {
        return BTreeMap::new();
    }
    let mut json_paths = Vec::new();
    for entry in walkdir::WalkDir::new(&coverage).into_iter().flatten() {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("json")
        {
            json_paths.push(entry.path().to_path_buf());
        }
    }
    json_paths.sort();
    let mut merged: BTreeMap<String, SwiftFile> = BTreeMap::new();
    for text in merge_texts(json_paths) {
        let Ok(parsed) = parse_swift_coverage(&text) else {
            continue;
        };
        for (key, record) in parsed {
            let entry = merged.entry(key).or_default();
            entry.regions.extend(record.regions);
            entry.summaries.extend(record.summaries);
        }
    }
    merged
}

/// Load merged LCOV line and branch records under `root`.
pub fn load_lcov(root: &Path) -> BTreeMap<String, FileCoverage> {
    merge_lcov(root)
}

/// Load Go coverage segments under `root`, or `None` if absent/empty.
pub fn load_go_profile(root: &Path) -> Option<BTreeMap<String, Vec<GoSegment>>> {
    let profile = merge_go(root);
    if profile.is_empty() {
        None
    } else {
        Some(profile)
    }
}

/// Load Cloverage HTML form coverage under `root`.
pub fn load_form_html(root: &Path) -> BTreeMap<String, BTreeMap<usize, (usize, usize)>> {
    merge_forms(root)
}

/// Load Swift llvm-cov/xccov coverage under `root`.
pub fn load_swift(root: &Path) -> BTreeMap<String, SwiftFile> {
    merge_swift(root)
}

pub fn load_bundle(root: &Path) -> CoverageBundle {
    let jacoco = merge_jacoco(root);
    CoverageBundle {
        lcov: load_lcov(root),
        go_profile: load_go_profile(root),
        jacoco: if jacoco.is_empty() {
            None
        } else {
            Some(jacoco)
        },
        form_html: load_form_html(root),
        swift: load_swift(root),
        source_parts: Vec::new(),
        root: Some(root.to_path_buf()),
    }
}

/// Load Java `<sourcefile><line ...>` covered lines across all JaCoCo reports under `root`.
pub fn load_jacoco_source_lines(root: &Path) -> BTreeMap<String, BTreeSet<usize>> {
    let paths = subdirs_1_and_2(root, &["target", "site", "jacoco", "jacoco.xml"]);
    let mut merged = BTreeMap::new();
    for text in merge_texts(paths) {
        if let Ok(parsed) = parse_jacoco_source_lines(&text) {
            merged.extend(parsed);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fn(language: &str, path: &str, start: usize, end: usize) -> Function {
        Function::new("choose", "demo.core", 2, start, end, path, language)
    }

    #[test]
    fn normalize_and_suffix_match() {
        assert_eq!(
            normalize_path("file:./src//demo/core.clj"),
            "src/demo/core.clj"
        );
        assert_eq!(normalize_path("%2E%2Fsrc%2Fcore.clj"), "src/core.clj");
        assert!(suffix_match("src/demo/core.clj", "demo/core.clj"));
        assert!(!suffix_match("core.clj", "src/demo/core.clj"));
        assert!(!suffix_match("src/demo/core.clj", ""));
        assert!(suffix_match("src/a.go", "src/a.go"));
    }

    #[test]
    fn cloverage_and_lcov_branch_priority() {
        let html = '<'.to_string()
            + "span class=\"pc\" title=\"1 out of 2 forms covered\">   4&nbsp;&nbsp;(if x 1)</span>";
        let parsed_html = parse_form_coverage(&html);
        assert_eq!(parsed_html.get(&4), Some(&(1, 2)));
        let lcov = parse_lcov("SF:src/demo/core.clj\nDA:3,1\nDA:4,0\nend_of_record\n");
        let mut form_html = BTreeMap::new();
        form_html.insert("src/demo/core.clj".to_string(), parsed_html);
        let bundle = CoverageBundle {
            lcov,
            form_html,
            ..Default::default()
        };
        assert_eq!(
            bundle.percent_for(&sample_fn("clojure", "src/demo/core.clj", 3, 5)),
            Some(50.0)
        );
    }

    #[test]
    fn lcov_branches_override_lines_when_present() {
        let text =
            "SF:src/demo/app.ts\nDA:2,1\nDA:3,1\nBRDA:3,0,0,1\nBRDA:3,0,1,-\nend_of_record\n";
        let parsed = parse_lcov(text);
        let bundle = CoverageBundle {
            lcov: parsed,
            ..Default::default()
        };
        assert_eq!(
            bundle.percent_for(&sample_fn("typescript", "src/demo/app.ts", 2, 3)),
            Some(50.0)
        );
    }

    #[test]
    fn go_profile_and_disambiguation() {
        let profile =
            parse_go_profile("mode: set\na/b/main.go:1.1,2.2 1 1\nb/main.go:1.1,2.2 1 0\n")
                .unwrap();
        assert_eq!(
            go_percent(Some(&profile), "b/main.go", 1, 2, None),
            Some(0.0)
        );
        assert_eq!(
            go_percent(Some(&profile), "a/b/main.go", 1, 2, None),
            Some(100.0)
        );
        assert!(parse_go_profile("mode: set\nbad\n").is_err());
    }

    #[test]
    fn jacoco_and_swift_innermost_region() {
        let xml = r#"<!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd">
        <report><package>
          <class name="demo/Board">
            <method name="place" line="4"><counter type="INSTRUCTION" missed="1" covered="3"/></method>
          </class>
        </package></report>"#;
        let bundle = CoverageBundle {
            jacoco: Some(parse_jacoco_index(xml).unwrap()),
            ..Default::default()
        };
        let mut f = sample_fn("java", "Board.java", 4, 6);
        f.name = "place".to_string();
        f.namespace = "demo.Board".to_string();
        f.jacoco_class = Some("demo.Board".to_string());
        assert_eq!(bundle.percent_for(&f), Some(75.0));

        let swift = SwiftFile {
            regions: vec![(1, 6, 1), (4, 5, 0)],
            summaries: vec![],
        };
        assert!(swift.line_covered(1));
        assert!(!swift.line_covered(4));
        assert!(!swift.line_covered(5));
        assert!(swift.line_covered(6));
    }

    #[test]
    fn lcov_checksum_brda_and_report_skip() {
        let text = "SF:src/a.ts\nDA:1,2,checksum123\nDA:2,0,abc\nDA:+3,1\nBRDA:2,0,0,-\nBRDA:2,0,1,4\nend_of_record\n";
        let parsed = parse_lcov(text);
        let rec = parsed.get("src/a.ts").unwrap();
        assert_eq!(rec.lines.get(&1), Some(&(1, 1)));
        assert_eq!(rec.lines.get(&2), Some(&(0, 1)));
        assert!(!rec.lines.contains_key(&3));
        assert_eq!(rec.branches.get(&2), Some(&(1, 2)));

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("crapper_lcov_skip_{}_{nanos}", std::process::id()));
        let report_lcov = root.join("coverage").join("lcov-report").join("lcov.info");
        let real_lcov = root.join("coverage").join("lcov.info");
        fs::create_dir_all(report_lcov.parent().unwrap()).unwrap();
        fs::write(&report_lcov, "SF:ignored.ts\nDA:1,1\nend_of_record\n").unwrap();
        fs::write(&real_lcov, "SF:kept.ts\nDA:1,1\nend_of_record\n").unwrap();
        let loaded = load_lcov(&root);
        assert!(loaded.contains_key("kept.ts"));
        assert!(!loaded.contains_key("ignored.ts"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_lcov_prefers_newer_mtime_across_target_and_coverage_dirs() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("crapper_lcov_mtime_{}_{nanos}", std::process::id()));
        let target_lcov = root
            .join("target")
            .join("coverage")
            .join("ts")
            .join("lcov.info");
        let cov_lcov = root.join("coverage").join("lcov.info");
        fs::create_dir_all(target_lcov.parent().unwrap()).unwrap();
        fs::create_dir_all(cov_lcov.parent().unwrap()).unwrap();

        fs::write(
            &target_lcov,
            "SF:src/shared.ts\nDA:1,1\nend_of_record\nSF:src/only_target.ts\nDA:1,1\nend_of_record\n",
        )
        .unwrap();
        fs::write(
            &cov_lcov,
            "SF:src/shared.ts\nDA:1,0\nend_of_record\nSF:src/only_cov.ts\nDA:1,1\nend_of_record\n",
        )
        .unwrap();

        let older =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let newer =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_010_000);
        let set_mtime = |p: &Path, t: std::time::SystemTime| {
            fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };

        // 1. Newer `target/coverage/.../lcov.info` overrides stale `coverage/lcov.info` on shared files
        //    while keeping disjoint records from both.
        set_mtime(&cov_lcov, older);
        set_mtime(&target_lcov, newer);
        let loaded_target_wins = load_lcov(&root);
        assert_eq!(
            loaded_target_wins["src/shared.ts"].lines.get(&1),
            Some(&(1, 1))
        );
        assert!(loaded_target_wins.contains_key("src/only_target.ts"));
        assert!(loaded_target_wins.contains_key("src/only_cov.ts"));

        // 2. Newer `coverage/lcov.info` overrides older `target/coverage/.../lcov.info`.
        set_mtime(&target_lcov, older);
        set_mtime(&cov_lcov, newer);
        let loaded_cov_wins = load_lcov(&root);
        assert_eq!(
            loaded_cov_wins["src/shared.ts"].lines.get(&1),
            Some(&(0, 1))
        );
        assert!(loaded_cov_wins.contains_key("src/only_target.ts"));
        assert!(loaded_cov_wins.contains_key("src/only_cov.ts"));

        let _ = fs::remove_dir_all(&root);
    }
}
