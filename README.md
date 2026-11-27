<div align="center">

# ub-tools-rs

Parity-verified pure Rust rewrites (`#![forbid(unsafe_code)]`) of Uncle Bob's quality-gate CLIs with incremental caching, structured JSON output, and multi-agent safeguards.

![Rust Edition 2024](https://img.shields.io/badge/Rust-Edition_2024-dea584?style=flat-square&logo=rust&logoColor=white)
![Unsafe Forbidden](https://img.shields.io/badge/unsafe-forbidden-2ea44f?style=flat-square)
![Tests 145 Passing](https://img.shields.io/badge/tests-145_passing-2ea44f?style=flat-square)
![License 0BSD and MIT](https://img.shields.io/badge/license-0BSD_%2F_MIT-0969da?style=flat-square)

[Quick Start](#quick-start) · [Workspace](#workspace) · [Agent Skills](#agent-skills) · [Safeguards](#safeguards) · [Benchmarks](#benchmarks) · [Configuration](#configuration)

</div>

```mermaid
flowchart LR
    Agent["AI Agents or Terminal"] --> Skills["skills/ub-mode and skills/ub-quality-gate"]
    Skills --> Tools["crapper, dryer, and mutator"]
    Agent --> Tools
    Tools --> Runtime["ub-runtime (priority, caps, locks, UBC1 cache)"]
    Runtime --> Outputs["Text reports, v1 JSON stdout, or .metrics/*.edn"]
```

## Quick Start

Requires Rust with edition `2024` support. Build and test the full workspace cleanly without a Python runtime.

```sh
cargo test --workspace
cargo build --release --workspace
```

Compiled binaries land under `target/release/` (`crapper`, `dryer`, `mutator`), or you can install them into Cargo's bin directory.

```sh
cargo install --path crates/crapper
cargo install --path crates/dryer
cargo install --path crates/mutator
```

Run read-only scoped checks from any project root with structured JSON output.

```sh
crapper --no-metrics --json --changed
dryer --no-metrics --json --threshold 0.8
mutator --scan --json --changed
```

## Workspace

| Crate | Scope and Languages | Output Artifacts | Upstream and PR |
| --- | --- | --- | --- |
| [`crates/crapper`](crates/crapper) | Cyclomatic complexity and coverage across 8 languages (Clojure, Java, Go, TypeScript/JS, Vue SFC, Rust, Python, Swift) | `.metrics/crap.edn`, text report, `v1` JSON | [unclebob/crapper](https://github.com/unclebob/crapper) ([PR #50](https://github.com/unclebob/crapper/pull/50)) |
| [`crates/dryer`](crates/dryer) | Structural code duplication via normalized AST fingerprints across the same 8 languages | `.metrics/dry.edn`, text, EDN, `v1` JSON | [unclebob/dryer](https://github.com/unclebob/dryer) ([PR #31](https://github.com/unclebob/dryer/pull/31)) |
| [`crates/mutator`](crates/mutator) | Coverage-guided mutation testing across 7 languages (Clojure, Java, Go, TypeScript/JS, Rust, Python, Swift), links `crapper_lib` | `.metrics/mutate/*.edn`, text, `--scan` / mutate `v1` JSON | [unclebob/mutator](https://github.com/unclebob/mutator) ([PR #47](https://github.com/unclebob/mutator/pull/47)) |
| [`crates/ub-runtime`](crates/ub-runtime) | Shared OS priority, thread/worker caps, global scan throttle, kernel file locks, atomic writes, and `UBC1` cache codec | Shared runtime library | Internal shared crate |

Companion upstream skill repo [unclebob/ub-mode](https://github.com/unclebob/ub-mode) serves as the base for [`skills/ub-mode/`](skills/ub-mode/README.md).

## Agent Skills

Bundled skills under `skills/` preconfigure AI coding agents to pass `--json`, `--changed`, and read-only `--no-metrics` flags automatically.

| Skill | Purpose | Default CLI Flags | License |
| --- | --- | --- | --- |
| [`skills/ub-mode/`](skills/ub-mode/README.md) | Incremental coding discipline across `crapper`, `dryer`, and `mutator` | `--json`, `--changed`, intermediate `--no-metrics`, `mutator --scan --json` | MIT ([LICENSE](skills/ub-mode/LICENSE)) |
| [`skills/ub-quality-gate/`](skills/ub-quality-gate/README.md) | Read-only quality verification after scoped code changes | `--no-metrics --json --changed`, `dryer --threshold 0.8`, `mutator --scan --json` | 0BSD ([LICENSE](LICENSE)) |

Install both skills into your agent's skills directory (for example `~/.claude/skills/`, `~/.codex/skills/`, `~/.cursor/skills/`, `~/.gemini/skills/`, or `~/.grok/skills/`).

```sh
cp -r skills/ub-mode skills/ub-quality-gate ~/.claude/skills/
```

```powershell
Copy-Item -Recurse skills\ub-mode, skills\ub-quality-gate "$HOME\.claude\skills\"
```

## Safeguards

> [!TIP]
> All three CLIs initialize `ub-runtime` automatically so concurrent multi-agent tool calls keep host editors responsive and prevent `.metrics/` corruption.

* **Lowered OS priority** runs processes at `Below Normal` on Windows by default (`UB_TOOLS_PRIORITY`), and spawned test or coverage subprocesses inherit that priority.
* **Bounded CPU pools** cap `rayon` threads at `min(6, cores/2)` (at least `1`) and `mutator` workers at `min(4, cores/4)` (at least `1`).
* **Cross-process scan slots** limit concurrent `crapper`, `dryer`, and `mutator --scan` runs to `2` global slots via OS locks under `ub-tools-locks/` in the system temp directory.
* **Repository kernel locks and atomic `.edn` writes** serialize per-repo coverage runs, mutation runs, and `.metrics/mutate/*.edn` updates while staging writes in unique sibling temp files with Windows sharing retries.
* **Report-only mode** via `--no-metrics` (or `UB_TOOLS_NO_METRICS`) prints `crapper` and `dryer` reports and preserves threshold exit codes without touching `.metrics/`.

### Incremental caching and JSON output

* **Per-repository `UBC1` caches** store parsed AST complexity records in `crapper`, compact 128-bit truncated SHA-256 subtree fingerprint hashes in `dryer`, and discovered mutation sites in `mutator`, placed outside repositories under the cache directory.
* **Stamps and zero-write warm hits** invalidate per file via `(mtime_ns, size)` plus nearest `go.mod`, `Cargo.toml`, and `Package.swift` manifest stamps in `crapper` and `mutator`; warm runs with zero misses skip rewriting cache files and keep exact stdout parity.
* **Bypass and structured JSON** support `--no-cache` (or `UB_TOOLS_NO_CACHE=1`) and emit versioned `v1` JSON via `--json` (plus `--format json` in `crapper` and `dryer`) while keeping exit codes and stderr diagnostics unchanged.

### Core Rust optimizations

* Parallel file discovery, parsing, and AST extraction via `rayon` with pruned directory traversal (`SKIP_DIRS`, `lcov-report`).
* Prefix-filtered Jaccard similarity in `dryer` with compact integer token interning, frequency ordering, length-ratio bounds, and two-pointer early-exit intersection.
* Zero-regex LCOV parsing with basename buckets in `crapper`, plus `RwLock` + `NormalizedIndex` coverage caching, single-pass `.metrics/mutate/*.edn` history indexing, and skipped SHA-256 form hashing without prior history in `mutator`.

## Benchmarks

### Cold vs. warm incremental cache

Measured on a 3,559-file static TypeScript monorepo snapshot with warm OS page cache (median of 3 runs per mode, `UB_TOOLS_CACHE_DIR` in an isolated temp directory, 100% byte-for-byte text parity across all modes).

| Workload | `--no-cache` | Cold cache | Warm cache | Speedup vs. uncached | Cache size |
| --- | ---: | ---: | ---: | ---: | ---: |
| `crapper` full `src` (`--no-coverage --no-metrics`, 12,861 fns) | 903.0 ms | 1,046.1 ms | 171.9 ms | 5.25x | 1.61 MB |
| `dryer` full `src` (`--threshold 0.8 --no-metrics`, 4,397 dups) | 1,923.2 ms | 4,488.8 ms | 248.3 ms | 7.75x | 15.00 MB |
| `mutator --scan` full `src` (`--no-coverage`, 66,259 sites) | 1,862.4 ms | 2,040.1 ms | 306.4 ms | 6.08x | 16.07 MB |

### Python baseline vs. Rust CLI

Measured on a static snapshot of a 3,522-file TypeScript monorepo (`src/domain` holds 217 files and 811 functions; full `src` holds 2,097 non-test files and 10,173 functions; coverage comes from 3.5 MB plus 5.4 MB `lcov.info` files alongside ~5,500 HTML files under `lcov-report`). Rust CLI timings include process startup and file output.

| Workload | Python before | Python after (PRs) | Rust CLI (`ub-tools-rs`) |
| --- | ---: | ---: | ---: |
| `crapper` `src/domain` (`--no-coverage`) | 511.2 ms (in-process) | 701.3 ms (in-process) | 119.2 ms |
| `crapper` full `src` (`--no-coverage`) | 25,330.0 ms (in-process) | 6,453.0 ms (in-process) | 1,238.0 ms |
| `crapper` `src/domain` (with coverage) | 4,387.3 ms (in-process) | 1,566.8 ms (in-process) | 850.3 ms |
| `crapper` full `src` (with coverage) | 40,888.2 ms (in-process) | 5,988.6 ms (in-process) | 6,745.6 ms |
| `dryer` `src/domain` (`--threshold 0.8`) | 3,373.0 ms | 1,016.5 ms | 190.1 ms |
| `dryer` full `src` (`--threshold 0.8`) | timed out (>60s earlier) | 12,373.4 ms | 2,108.3 ms |
| `mutator --scan` `src/types` (with coverage) | timed out (>30s) | 1,113.3 ms | 155.2 ms |
| `mutator --scan` `src/domain` (`--no-coverage`) | 5,704.9 ms | 3,206.9 ms | 368.5 ms |
| `mutator --scan` full `src` (with coverage) | not run | not run | 4,519.9 ms |

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `UB_TOOLS_PRIORITY` | `below_normal` | Process priority (`below_normal`, `low`, `idle`, or `normal`) for CLIs and children |
| `UB_TOOLS_THREADS` | `min(6, cores/2)` | Override `rayon` global thread pool size (falls back to `RAYON_NUM_THREADS` when unset) |
| `UB_TOOLS_MUTATOR_WORKERS` | `min(4, cores/4)` | Override default `mutator` worker count when `--max-workers` is omitted |
| `UB_TOOLS_MAX_CONCURRENT` | `2` | Global scan lock slots across `crapper`, `dryer`, and `mutator --scan` (`0` disables) |
| `UB_TOOLS_NO_METRICS` | unset | Skip writing `.metrics/crap.edn` and `.metrics/dry.edn` when set to `1`, `true`, or `yes` |
| `UB_TOOLS_CACHE_DIR` | OS cache dir (`%LOCALAPPDATA%\ub-tools-cache` or `~/.cache/ub-tools-cache`) | Override directory storing per-repository `UBC1` incremental cache files |
| `UB_TOOLS_NO_CACHE` | unset | Disable reading and writing `UBC1` incremental cache files when set to `1`, `true`, or `yes` |

## License

The Rust workspace and [`skills/ub-quality-gate/`](skills/ub-quality-gate/README.md) use the BSD Zero Clause License (`0BSD`) preserving upstream attribution to Robert C. Martin in [LICENSE](LICENSE), while [`skills/ub-mode/`](skills/ub-mode/README.md) preserves its upstream MIT License in [skills/ub-mode/LICENSE](skills/ub-mode/LICENSE).
