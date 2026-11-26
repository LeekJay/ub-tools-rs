use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

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

pub fn analyze_files(
    files: &[PathBuf],
    project_root: &Path,
    mut bundle: Option<&mut CoverageBundle>,
) -> Vec<Entry> {
    let root = resolve_path(project_root);
    let root_str = root.to_string_lossy().into_owned();
    let resolved_files: Vec<PathBuf> = files.par_iter().map(|f| resolve_path(f)).collect();
    if let Some(b) = bundle.as_deref_mut() {
        let paths: Vec<String> = resolved_files
            .iter()
            .map(|resolved| source_path(resolved, &root))
            .collect();
        b.bind_sources(&paths);
    }
    let bundle_ref = bundle.as_deref();
    let entries: Vec<Entry> = resolved_files
        .par_iter()
        .flat_map(|resolved| {
            let Some(language) = language_of(resolved) else {
                return Vec::new();
            };
            let Ok(bytes) = fs::read(resolved) else {
                return Vec::new();
            };
            let source = String::from_utf8_lossy(&bytes);
            let relative = source_path(resolved, &root);
            functions_in_file(language, &source, &relative, Some(root_str.as_str()))
                .into_iter()
                .map(|function| make_entry(&function, coverage_for(bundle_ref, &function)))
                .collect()
        })
        .collect();
    sort_entries(&entries)
}
