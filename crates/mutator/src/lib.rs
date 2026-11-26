//! Pure Rust mutation testing library replacing Python `mutator` and `crapper_link.py`.

#![forbid(unsafe_code)]

pub mod cli;
pub mod coverage;
pub mod crapper_bridge;
pub mod edn;
pub mod engine;
pub mod functions;
pub mod metrics;
pub mod model;
pub mod path_util;
pub mod report;
pub mod runner;
pub mod sites;
pub mod sources;
pub mod workers;
