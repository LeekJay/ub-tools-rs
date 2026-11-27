//! Core data types shared across language extraction, comparison, and reporting.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Where one candidate form sits in a source file (1-based inclusive lines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// One pair of forms whose normalized structure meets the similarity threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Duplicate {
    pub score: f64,
    pub language: String,
    pub left: Span,
    pub right: Span,
    pub left_nodes: usize,
    pub right_nodes: usize,
}

/// A normalized form ready to compare within a single language pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub language: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub nodes: usize,
    pub fingerprints: HashSet<String>,
}
