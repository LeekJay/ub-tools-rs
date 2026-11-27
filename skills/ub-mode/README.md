[Back to ub-tools-rs](../../README.md)

# ub-mode

Disciplined incremental coding skill pairing Uncle Bob's three-phase workflow with the pure Rust `crapper`, `dryer`, and `mutator` CLIs.

![Skill /ub-mode](https://img.shields.io/badge/skill-%2Fub--mode-0969da?style=flat-square)
![License MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)

Modeled on [unclebob/ub-mode](https://github.com/unclebob/ub-mode) and loosely based on portions of [@poteto](https://github.com/poteto)'s rules (`poteto-mode` / `pstack`).

## Workflow Phases

1. **First Make it Work** covers Laziness Protocol, Foundational Thinking, TDD, Redesign From First Principles, Attack the Premise, Subtract Before You Add, Outcome-Oriented Execution, Build the Lever, and Architecture (Model the Domain, Boundary Discipline, Type System Discipline, Make Operations Idempotent, Separate Before Serializing Shared State).
2. **Then Make it Right** covers Fix Root Causes, Sequence Work into Verifiable Units, Test Behavior (Not Implementation), `crapper` (`CRAP < 10`), `dryer` (duplication `< 0.8`), `mutator` (cover the uncovered, kill the survivors, delete dead code), and Refactoring.
3. **Practice** covers Keep a Log, Guard the Context Window, Worktree Cleanup, and Commit and Push in small ordered commits.

## Companion Tools and Flags

`SKILL.md` preconfigures the companion CLIs with `ub-tools-rs` flags so agents scope checks to changed files and parse structured JSON without leaving noisy artifacts behind.

| Tool | Local Crate | Configured Flags in `SKILL.md` | Upstream |
| --- | --- | --- | --- |
| `crapper` | [`crates/crapper`](../../crates/crapper) | `--json`, `--changed` or target paths, `--no-metrics` for intermediate checks | [unclebob/crapper](https://github.com/unclebob/crapper) |
| `dryer` | [`crates/dryer`](../../crates/dryer) | `--threshold 0.8 --json`, `--no-metrics` for intermediate checks | [unclebob/dryer](https://github.com/unclebob/dryer) |
| `mutator` | [`crates/mutator`](../../crates/mutator) | `--scan --json` first to filter out non-decision lines, then `--json` with `--lines` and scoped `--test-command` for slow runners | [unclebob/mutator](https://github.com/unclebob/mutator) |
| `uml-viewer` | External companion | Visualizes `.metrics/` components, coverage, CRAP, mutation scores, and dependencies | [unclebob/uml-viewer](https://github.com/unclebob/uml-viewer) |

## Installation

Copy `skills/ub-mode` into your agent's skills directory (for example `~/.claude/skills/ub-mode/`, `~/.codex/skills/ub-mode/`, `~/.cursor/skills/ub-mode/`, `~/.gemini/skills/ub-mode/`, or `~/.grok/skills/ub-mode/`), then invoke `/ub-mode`.

```sh
cp -r skills/ub-mode ~/.claude/skills/
```

```powershell
Copy-Item -Recurse skills\ub-mode "$HOME\.claude\skills\"
```

## License

Preserves upstream MIT License attribution to Robert C. Martin in [LICENSE](LICENSE).
