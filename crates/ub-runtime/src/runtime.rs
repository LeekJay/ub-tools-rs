use std::num::NonZeroUsize;

use crate::priority::apply_current_process_priority;

const ENV_THREADS: &str = "UB_TOOLS_THREADS";
const ENV_RAYON_THREADS: &str = "RAYON_NUM_THREADS";
const ENV_MUTATOR_WORKERS: &str = "UB_TOOLS_MUTATOR_WORKERS";
const ENV_NO_METRICS: &str = "UB_TOOLS_NO_METRICS";

const MIN_RAYON_THREADS: usize = 1;
const MAX_RAYON_THREADS: usize = 6;
const MIN_MUTATOR_WORKERS: usize = 1;
const MAX_DEFAULT_MUTATOR_WORKERS: usize = 4;

fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1)
}

fn parse_positive_usize(raw: Option<&str>) -> Option<usize> {
    let trimmed = raw?.trim();
    let parsed = trimmed.parse::<usize>().ok()?;
    (parsed > 0).then_some(parsed)
}

pub(crate) fn resolve_global_rayon_threads(
    ub_tools_threads: Option<&str>,
    rayon_num_threads: Option<&str>,
    cores: usize,
) -> Option<usize> {
    if let Some(explicit) = parse_positive_usize(ub_tools_threads) {
        return Some(explicit);
    }
    if rayon_num_threads.is_some_and(|value| !value.trim().is_empty()) {
        return None;
    }
    Some(resolve_rayon_threads(cores))
}

/// Initializes process priority and configures Rayon's global thread pool once.
///
/// Sets current OS process priority (no-op on non-Windows or when `UB_TOOLS_PRIORITY=normal`),
/// then configures Rayon using `UB_TOOLS_THREADS`, leaving it to `RAYON_NUM_THREADS` when set
/// and non-empty, or falling back to `resolve_rayon_threads(available_cores)`.
pub fn init_cli_runtime() {
    apply_current_process_priority();

    let ub_threads = std::env::var(ENV_THREADS).ok();
    let rayon_threads = std::env::var(ENV_RAYON_THREADS).ok();
    if let Some(num_threads) = resolve_global_rayon_threads(
        ub_threads.as_deref(),
        rayon_threads.as_deref(),
        available_cores(),
    ) {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build_global();
    }
}

/// Returns the default Rayon thread count for a host with `cores` logical CPUs.
pub fn resolve_rayon_threads(cores: usize) -> usize {
    (cores / 2).clamp(MIN_RAYON_THREADS, MAX_RAYON_THREADS)
}

pub(crate) fn default_mutator_workers_from(cores: usize, raw_override: Option<&str>) -> usize {
    if let Some(explicit) = parse_positive_usize(raw_override) {
        return explicit.clamp(MIN_MUTATOR_WORKERS, cores.max(1));
    }
    (cores / 4).clamp(MIN_MUTATOR_WORKERS, MAX_DEFAULT_MUTATOR_WORKERS)
}

/// Returns the default mutator worker count for `cores`, honoring `UB_TOOLS_MUTATOR_WORKERS`.
pub fn default_mutator_workers(cores: usize) -> usize {
    let raw = std::env::var(ENV_MUTATOR_WORKERS).ok();
    default_mutator_workers_from(cores, raw.as_deref())
}

pub(crate) fn parse_env_no_metrics(raw: Option<&str>) -> bool {
    let Some(value) = raw else {
        return false;
    };
    let trimmed = value.trim();
    trimmed == "1" || trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("yes")
}

/// Returns `true` when `UB_TOOLS_NO_METRICS` is `"1"`, `"true"`, or `"yes"` (trimmed, case-insensitive).
pub fn env_no_metrics() -> bool {
    let raw = std::env::var(ENV_NO_METRICS).ok();
    parse_env_no_metrics(raw.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_rayon_threads_across_core_counts() {
        assert_eq!(resolve_rayon_threads(0), 1);
        assert_eq!(resolve_rayon_threads(1), 1);
        assert_eq!(resolve_rayon_threads(2), 1);
        assert_eq!(resolve_rayon_threads(3), 1);
        assert_eq!(resolve_rayon_threads(4), 2);
        assert_eq!(resolve_rayon_threads(8), 4);
        assert_eq!(resolve_rayon_threads(11), 5);
        assert_eq!(resolve_rayon_threads(12), 6);
        assert_eq!(resolve_rayon_threads(16), 6);
        assert_eq!(resolve_rayon_threads(64), 6);
    }

    #[test]
    fn resolves_global_rayon_thread_precedence() {
        assert_eq!(
            resolve_global_rayon_threads(Some(" 3 "), Some("8"), 16),
            Some(3)
        );
        assert_eq!(resolve_global_rayon_threads(Some("0"), Some("8"), 16), None);
        assert_eq!(
            resolve_global_rayon_threads(Some("invalid"), Some(" 5 "), 16),
            None
        );
        assert_eq!(resolve_global_rayon_threads(None, Some("   "), 16), Some(6));
        assert_eq!(resolve_global_rayon_threads(None, None, 16), Some(6));
    }

    #[test]
    fn resolves_default_mutator_workers_with_and_without_override() {
        assert_eq!(default_mutator_workers_from(0, None), 1);
        assert_eq!(default_mutator_workers_from(1, None), 1);
        assert_eq!(default_mutator_workers_from(4, None), 1);
        assert_eq!(default_mutator_workers_from(8, None), 2);
        assert_eq!(default_mutator_workers_from(12, None), 3);
        assert_eq!(default_mutator_workers_from(16, None), 4);
        assert_eq!(default_mutator_workers_from(32, None), 4);

        assert_eq!(default_mutator_workers_from(16, Some(" 7 ")), 7);
        assert_eq!(default_mutator_workers_from(4, Some("10")), 4);
        assert_eq!(default_mutator_workers_from(0, Some("3")), 1);
        assert_eq!(default_mutator_workers_from(16, Some("0")), 4);
        assert_eq!(default_mutator_workers_from(16, Some("bad")), 4);
    }

    #[test]
    fn parses_no_metrics_flag_variants() {
        assert!(!parse_env_no_metrics(None));
        assert!(!parse_env_no_metrics(Some("")));
        assert!(!parse_env_no_metrics(Some("0")));
        assert!(!parse_env_no_metrics(Some("false")));
        assert!(!parse_env_no_metrics(Some("no")));
        assert!(!parse_env_no_metrics(Some("off")));
        assert!(parse_env_no_metrics(Some("1")));
        assert!(parse_env_no_metrics(Some(" 1 ")));
        assert!(parse_env_no_metrics(Some("true")));
        assert!(parse_env_no_metrics(Some(" TRUE ")));
        assert!(parse_env_no_metrics(Some("yes")));
        assert!(parse_env_no_metrics(Some(" Yes ")));
    }
}
