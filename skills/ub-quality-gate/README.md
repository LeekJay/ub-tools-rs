# ub-quality-gate

`ub-quality-gate` is a read-only verification skill for scoped code changes. It acts strictly as an objective verifier (never modifying code, installing tools, committing, or running a full coding workflow) and pairs with the pure Rust `ub-tools-rs` CLIs.

## Verification workflow

* Classifies change risk (Low, Medium, or High) from the working tree, diff, or branch comparison.
* Runs relevant tests, complexity checks (`crapper`), duplication checks (`dryer`), and targeted mutation checks (`mutator`) matched to the repository's actual setup.
* Reports `PASS`, `FAIL`, `NOT_RUN`, or `N/A` per gate so missing coverage or unrun checks never pass silently.

## Read-only `ub-tools-rs` flags

`SKILL.md` specifies non-mutating flags so verification runs leave `.metrics/` and `git status` clean.

* Complexity check uses `crapper --no-metrics --json` scoped with `--changed` or explicit paths (`--use-existing-coverage` only when up-to-date coverage is already on disk).
* Duplication check uses `dryer --no-metrics --json --threshold 0.8` across the relevant source root so duplicates against unchanged code are caught, focusing findings on changed files.
* Mutation check starts read-only with `mutator --scan --json` (`--changed` or explicit paths), and passes `--json` when running targeted mutations (note that full mutation runs write `.metrics/mutate/*.edn`).

## Installation

Copy `SKILL.md` into your agent's skills directory as `ub-quality-gate/SKILL.md` (for example `~/.claude/skills/ub-quality-gate/SKILL.md`, `~/.codex/skills/ub-quality-gate/SKILL.md`, `~/.cursor/skills/ub-quality-gate/SKILL.md`, `~/.gemini/skills/ub-quality-gate/SKILL.md`, or `~/.grok/skills/ub-quality-gate/SKILL.md`), then invoke `/ub-quality-gate`.

## License

Licensed under `0BSD` in [../../LICENSE](../../LICENSE).
