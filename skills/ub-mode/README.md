# ub-mode

`ub-mode` is an agent skill that enforces disciplined software engineering principles when writing or changing code. Modeled on [unclebob/ub-mode](https://github.com/unclebob/ub-mode) and loosely based on portions of [@poteto](https://github.com/poteto)'s rules (`poteto-mode` / `pstack`), it pairs a reduced workflow with deterministic quality-gate CLIs (`crapper`, `dryer`, and `mutator`).

1. **First Make it Work**, covering Laziness Protocol, Foundational Thinking, TDD, Redesign From First Principles, Attack the Premise, Subtract Before You Add, Outcome-Oriented Execution, Build the Lever, and Architecture (Model the Domain, Boundary Discipline, Type System Discipline, Make Operations Idempotent, Separate Before Serializing Shared State).
2. **Then Make it Right**, covering Fix Root Causes, Sequence Work into Verifiable Units, Test Behavior (Not Implementation), `crapper` (`CRAP < 10`), `dryer` (duplication `< 0.8`), `mutator` (cover the uncovered, kill the survivors, delete dead code), and Refactoring.
3. **Practice**, covering Keep a Log, Guard the Context Window, Worktree Cleanup, and Commit and Push in small ordered commits.

## Companion tools and flags

`SKILL.md` configures the companion CLIs with `ub-tools-rs` workflow flags so agents inspect compact JSON results scoped to changed files without cluttering `.metrics/`.

* [crates/crapper](../../crates/crapper) ([unclebob/crapper](https://github.com/unclebob/crapper)) computes cyclomatic complexity, test coverage, and CRAP scores (`--json`, `--changed`, and `--no-metrics` for intermediate checks).
* [crates/dryer](../../crates/dryer) ([unclebob/dryer](https://github.com/unclebob/dryer)) detects structural code duplication (`--threshold 0.8 --json`, and `--no-metrics` for intermediate checks).
* [crates/mutator](../../crates/mutator) ([unclebob/mutator](https://github.com/unclebob/mutator)) discovers and runs mutation tests (`--scan --json` first to inspect sites, then `--json` with `--changed` or target paths).
* [unclebob/uml-viewer](https://github.com/unclebob/uml-viewer) visualizes components, coverage, CRAP, mutation scores, and dependency directions.

## Installation

Copy `SKILL.md` into your agent's skills directory as `ub-mode/SKILL.md` (for example `~/.claude/skills/ub-mode/SKILL.md`, `~/.codex/skills/ub-mode/SKILL.md`, `~/.cursor/skills/ub-mode/SKILL.md`, `~/.gemini/skills/ub-mode/SKILL.md`, or `~/.grok/skills/ub-mode/SKILL.md`), then invoke `/ub-mode`.

## License

Preserves upstream MIT License attribution to Robert C. Martin in [LICENSE](LICENSE).
