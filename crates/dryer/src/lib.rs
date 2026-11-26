//! Pure Rust implementation of the `dryer` structural duplicate finder.

#![forbid(unsafe_code)]

pub mod astnorm;
pub mod cli;
pub mod clojure_read;
pub mod discover;
pub mod extract;
pub mod model;
pub mod report;
pub mod scan;
pub mod shape;
pub mod treesitter;
pub mod vue;
