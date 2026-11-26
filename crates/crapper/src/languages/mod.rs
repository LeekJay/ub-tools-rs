pub mod clojure;
pub mod golang;
pub mod java;
pub mod python;
pub mod rust;
pub mod swift;
pub mod treesitter;
pub mod typescript;
pub mod vue;

use crate::model::Function;

pub fn functions_in_file<'a>(
    language: &str,
    source: &str,
    path: &str,
    project_root: impl Into<Option<&'a str>>,
) -> Vec<Function> {
    let root = project_root.into();
    match language {
        "clojure" => clojure::functions_in_source(source, path, root),
        "java" => java::functions_in_source(source, path, root),
        "go" => golang::functions_in_source(source, path, root),
        "typescript" => typescript::functions_in_source(source, path, root),
        "python" => python::functions_in_source(source, path, root),
        "rust" => rust::functions_in_source(source, path, root),
        "swift" => swift::functions_in_source(source, path, root),
        "vue" => vue::functions_in_source(source, path, root),
        _ => Vec::new(),
    }
}
