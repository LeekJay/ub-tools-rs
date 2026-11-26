use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create target dir");
    for entry in fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy fixture file");
        }
    }
}

fn is_out_snapshot_dir(path: &Path) -> bool {
    path.is_dir() && path.extension().is_some_and(|ext| ext == "out")
}

fn unquote_basic(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

fn unquote_multiline_basic(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    if chars.peek() == Some(&'\r') {
        chars.next();
    }
    if chars.peek() == Some(&'\n') {
        chars.next();
    }
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '"' && chars.peek() == Some(&'"') {
            let mut look = chars.clone();
            look.next();
            if look.peek() == Some(&'"') {
                chars.next();
                chars.next();
                while chars.peek() == Some(&'"') {
                    out.push('"');
                    chars.next();
                }
                break;
            }
        }
        out.push(ch);
    }
    out
}

fn unquote_literal(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut out = String::new();
    for ch in chars.by_ref() {
        if ch == '\'' {
            break;
        }
        out.push(ch);
    }
    out
}

fn parse_toml_args(toml: &str) -> Vec<String> {
    let after_key = toml
        .lines()
        .find_map(|line| line.strip_prefix("args = "))
        .expect("args key in case toml");
    let mut chars = after_key.chars().peekable();
    assert_eq!(chars.next(), Some('['), "expected args array");
    let mut args = Vec::new();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        match chars.peek() {
            Some(']') | None => break,
            Some('"') => {
                chars.next();
                let mut look = chars.clone();
                if look.next() == Some('"') && look.next() == Some('"') {
                    chars.next();
                    chars.next();
                    args.push(unquote_multiline_basic(&mut chars));
                } else {
                    args.push(unquote_basic(&mut chars));
                }
            }
            Some('\'') => {
                chars.next();
                args.push(unquote_literal(&mut chars));
            }
            Some(other) => panic!("unexpected token in args array: {other}"),
        }
    }
    args
}

fn parse_toml_status(toml: &str) -> i32 {
    toml.lines()
        .find_map(|line| line.strip_prefix("status.code = "))
        .map(|s| s.trim().parse::<i32>().expect("integer status.code"))
        .unwrap_or(0)
}

fn normalize_tested_at(edn: &str) -> String {
    let marker = ":tested-at \"";
    let Some(start) = edn.find(marker) else {
        return edn.to_string();
    };
    let val_start = start + marker.len();
    let Some(rel_end) = edn[val_start..].find('"') else {
        return edn.to_string();
    };
    let mut out = String::with_capacity(edn.len());
    out.push_str(&edn[..val_start]);
    out.push_str("[..]");
    out.push_str(&edn[val_start + rel_end..]);
    out
}

fn collect_files_recursive(root: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read out dir") {
        let entry = entry.expect("out entry");
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, base, out);
        } else {
            let rel = path
                .strip_prefix(base)
                .expect("relative path")
                .to_path_buf();
            out.push(rel);
        }
    }
}

#[test]
fn recorded_edn_snapshots_match() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let bin = Path::new(env!("CARGO_BIN_EXE_mutator"));

    let mut out_dirs: Vec<PathBuf> = fs::read_dir(&fixtures)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_out_snapshot_dir(p))
        .collect();
    out_dirs.sort();
    assert_eq!(out_dirs.len(), 24, "expected 24 EDN snapshot fixtures");

    for out_dir in out_dirs {
        let stem = out_dir
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("out stem");
        let toml = fs::read_to_string(fixtures.join(format!("{stem}.toml"))).expect("read toml");
        let args = parse_toml_args(&toml);
        let expected_code = parse_toml_status(&toml);

        let sandbox = TempDirGuard::new("mutator_edn_snap");
        let in_dir = fixtures.join(format!("{stem}.in"));
        if in_dir.is_dir() {
            copy_dir_recursive(&in_dir, &sandbox.0);
        }

        let output = Command::new(bin)
            .args(&args)
            .current_dir(&sandbox.0)
            .output()
            .expect("run mutator binary");
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "case {stem} exit code mismatch; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !sandbox.0.join("target").join("mutation-workers").exists(),
            "leftover worker overlays in {stem}"
        );

        let mut expected_files = Vec::new();
        collect_files_recursive(&out_dir, &out_dir, &mut expected_files);
        assert!(!expected_files.is_empty(), "empty .out for {stem}");
        for rel in expected_files {
            let expected_raw = fs::read_to_string(out_dir.join(&rel)).expect("read expected file");
            let actual_raw = fs::read_to_string(sandbox.0.join(&rel))
                .unwrap_or_else(|e| panic!("missing {} in {stem}: {e}", rel.display()));
            assert!(
                actual_raw.contains(":tested-at \"20"),
                "expected timestamp in {stem}: {actual_raw}"
            );
            assert_eq!(
                normalize_tested_at(actual_raw.trim_end()),
                normalize_tested_at(expected_raw.trim_end()),
                "snapshot mismatch for {stem}/{}",
                rel.display()
            );
        }
    }
}
