use std::cmp::Ordering;

use crate::model::{Entry, Function};

/// CRAP = CC^2 * (1 - coverage)^3 + CC.
///
/// `coverage_pct` is a percentage from 0 to 100. Missing coverage yields no
/// score, which the report shows as N/A.
pub fn crap_score(complexity: usize, coverage_pct: Option<f64>) -> Option<f64> {
    let cov = coverage_pct?;
    let cc = complexity as f64;
    let uncovered = 1.0 - (cov / 100.0);
    Some((cc * cc * uncovered * uncovered * uncovered) + cc)
}

/// Round to 4 decimal places using round-half-to-even to match Python `round(x, 4)`.
pub fn round_metric(value: Option<f64>) -> Option<f64> {
    let v = value?;
    if !v.is_finite() {
        return Some(v);
    }
    let scaled = v * 10_000.0;
    let floor = scaled.floor();
    let frac = scaled - floor;
    let rounded = if (frac - 0.5).abs() < 1e-12 {
        // Tie: pick even integer.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    let out = rounded / 10_000.0;
    Some(if out == 0.0 { 0.0 } else { out })
}

pub fn make_entry(function: &Function, coverage: Option<f64>) -> Entry {
    let rounded_cov = round_metric(coverage);
    let score = round_metric(crap_score(function.complexity, rounded_cov));
    Entry {
        name: function.name.clone(),
        namespace: function.namespace.clone(),
        complexity: function.complexity,
        coverage: rounded_cov,
        crap: score,
        start_line: function.start_line,
        end_line: function.end_line,
        path: function.path.clone(),
        language: function.language.clone(),
    }
}

fn compare_entries(a: &Entry, b: &Entry) -> Ordering {
    let key_rank = |entry: &Entry| if entry.crap.is_some() { 0u8 } else { 1u8 };
    key_rank(a)
        .cmp(&key_rank(b))
        .then_with(|| match (a.crap, b.crap) {
            (Some(ca), Some(cb)) => cb.total_cmp(&ca),
            (None, None) => b.complexity.cmp(&a.complexity),
            _ => Ordering::Equal,
        })
        .then_with(|| a.namespace.cmp(&b.namespace))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.start_line.cmp(&b.start_line))
}

/// Worst CRAP first. Unscored rows follow scored rows.
pub fn sort_entries(entries: &[Entry]) -> Vec<Entry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(compare_entries);
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str, complexity: usize, crap: Option<f64>) -> Entry {
        Entry {
            name: name.to_string(),
            namespace: "demo.core".to_string(),
            complexity,
            coverage: crap.map(|_| 100.0),
            crap,
            start_line: 1,
            end_line: 5,
            path: "src/demo/core.clj".to_string(),
            language: "clojure".to_string(),
        }
    }

    #[test]
    fn formula_matches_crap4clj() {
        assert_eq!(crap_score(5, Some(100.0)), Some(5.0));
        assert_eq!(crap_score(5, Some(0.0)), Some(30.0));
        assert_eq!(crap_score(1, Some(100.0)), Some(1.0));
        let val = crap_score(8, Some(45.0)).unwrap();
        assert!((val - 18.648).abs() < 0.01);
        assert_eq!(crap_score(3, None), None);
    }

    #[test]
    fn sorting_puts_scored_worst_first_then_unscored_by_complexity() {
        let rows = sort_entries(&[
            sample_entry("missing", 1, None),
            sample_entry("mid", 1, Some(5.0)),
            sample_entry("worst", 1, Some(30.0)),
        ]);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["worst", "mid", "missing"]);

        let rows2 = sort_entries(&[
            sample_entry("small", 1, None),
            sample_entry("large", 8, None),
        ]);
        let names2: Vec<_> = rows2.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names2, ["large", "small"]);
    }
}
