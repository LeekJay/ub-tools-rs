#![forbid(unsafe_code)]

pub use tree_sitter;

pub mod analyze;
pub mod cache;
pub mod cli;
pub mod coverage;
pub mod crap;
pub mod discover;
pub mod languages;
pub mod metrics;
pub mod model;
pub mod report;
pub mod runners;

pub(crate) mod git_diff;
pub(crate) mod path_util;
