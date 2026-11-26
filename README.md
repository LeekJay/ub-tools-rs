# ub-tools-rs

Parity-verified pure Rust rewrites (`#![forbid(unsafe_code)]`) of Uncle Bob's three software metrics CLIs, collected in a single Cargo workspace so `mutator`, `crapper`, and `dryer` compile together cleanly without a Python runtime.

## Upstream projects and Python PRs

* [unclebob/crapper](https://github.com/unclebob/crapper) paired with performance PR [unclebob/crapper#50](https://github.com/unclebob/crapper/pull/50)
* [unclebob/dryer](https://github.com/unclebob/dryer) paired with performance PR [unclebob/dryer#31](https://github.com/unclebob/dryer/pull/31)
* [unclebob/mutator](https://github.com/unclebob/mutator) paired with performance PR [unclebob/mutator#47](https://github.com/unclebob/mutator/pull/47)

## Workspace crates

* `crates/crapper` scores cyclomatic complexity and coverage into `.metrics/crap.edn` across Clojure, Java, Go, TypeScript, JavaScript, Vue SFC, Rust, Python, and Swift.
* `crates/dryer` detects structural code duplicates via normalized AST fingerprints and Jaccard similarity into `.metrics/dry.edn`.
* `crates/mutator` runs coverage-guided mutation testing across `.metrics/mutate/*.edn` snapshots and links `crapper` coverage and language discovery directly as a library crate (`crapper_lib`).

## Build and test

Requires Rust with edition `2024` support.

```sh
cargo test --workspace
cargo build --release --workspace
```

Compiled binaries land under `target/release/` (`crapper`, `dryer`, `mutator`). You can also install individual tools locally.

```sh
cargo install --path crates/crapper
cargo install --path crates/dryer
cargo install --path crates/mutator
```

## Key optimizations

* Parallel file discovery, parsing, and AST extraction powered by `rayon`.
* Prefix-filtered Jaccard matching in `dryer` with compact integer token interning, frequency ordering, length-ratio bounds, and two-pointer early-exit intersection.
* Zero-regex LCOV parsing plus basename-bucketed key indexing and `SKIP_DIRS` / `lcov-report` directory pruning in `crapper`.
* Per-run coverage caching (`RwLock` + `NormalizedIndex`), single-pass `.metrics/mutate/*.edn` history indexing, and skipped SHA-256 form hashing when no prior history exists in `mutator`.

## Measured benchmarks

Measured on a static snapshot of a real 3,522-file TypeScript monorepo (`src/domain` holds 217 files and 811 functions; full `src` holds 2,097 non-test files and 10,173 functions; coverage comes from 3.5 MB plus 5.4 MB `lcov.info` files alongside ~5,500 HTML files under `lcov-report`). All Rust CLI times include process startup and file output.

| Workload | Python before | Python after (PRs) | Rust CLI (`ub-tools-rs`) |
| --- | --- | --- | --- |
| `crapper` `src/domain` (`--no-coverage`) | 511.2 ms (in-process) | 701.3 ms (in-process) | 119.2 ms |
| `crapper` full `src` (`--no-coverage`) | 25,330.0 ms (in-process) | 6,453.0 ms (in-process) | 1,238.0 ms |
| `crapper` `src/domain` (with coverage) | 4,387.3 ms (in-process) | 1,566.8 ms (in-process) | 850.3 ms |
| `crapper` full `src` (with coverage) | 40,888.2 ms (in-process) | 5,988.6 ms (in-process) | 6,745.6 ms |
| `dryer` `src/domain` (`--threshold 0.8`) | 3,373.0 ms | 1,016.5 ms | 190.1 ms |
| `dryer` full `src` (`--threshold 0.8`) | timed out (>60s earlier) | 12,373.4 ms | 2,108.3 ms |
| `mutator --scan` `src/types` (with coverage) | timed out (>30s) | 1,113.3 ms | 155.2 ms |
| `mutator --scan` `src/domain` (`--no-coverage`) | 5,704.9 ms | 3,206.9 ms | 368.5 ms |
| `mutator --scan` full `src` (with coverage) | not run | not run | 4,519.9 ms |

## License

BSD Zero Clause License (`0BSD`). Preserves upstream copyright attribution to Robert C. Martin in [LICENSE](LICENSE).
