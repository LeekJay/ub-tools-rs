use serde::{Deserialize, Serialize};

/// One scored unit: a function or method.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub namespace: String,
    pub complexity: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub path: String,
    pub language: String,
    pub jacoco_class: Option<String>,
    /// UTF-8 byte span of the function. `None` when the language does not record it.
    /// Mutator uses the span to give a nested handler the sites inside it.
    pub start_byte: Option<usize>,
    pub end_byte: Option<usize>,
}

impl Function {
    pub fn new(
        name: impl Into<String>,
        namespace: impl Into<String>,
        complexity: usize,
        start_line: usize,
        end_line: usize,
        path: impl Into<String>,
        language: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            namespace: namespace.into(),
            complexity,
            start_line,
            end_line,
            path: path.into(),
            language: language.into(),
            jacoco_class: None,
            start_byte: None,
            end_byte: None,
        }
    }
}

/// One row of the CRAP report and of `.metrics/crap.edn`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub namespace: String,
    pub complexity: usize,
    pub coverage: Option<f64>,
    pub crap: Option<f64>,
    pub path: String,
    pub language: String,
}
