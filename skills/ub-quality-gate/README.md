[Back to ub-tools-rs](../../README.md)

# ub-quality-gate

Read-only evidence-based verification skill for scoped code changes, backed by the pure Rust `ub-tools-rs` CLIs.

![Skill /ub-quality-gate](https://img.shields.io/badge/skill-%2Fub--quality--gate-2ea44f?style=flat-square)
![License 0BSD](https://img.shields.io/badge/license-0BSD-blue?style=flat-square)

Acts strictly as an objective verifier (never modifying code, installing tools, committing, or running a second full coding workflow).

## Verification Workflow

* Classifies change risk (Low, Medium, or High) from the working tree, diff, or branch comparison.
* Runs relevant tests, CRAP complexity (`crapper`), structural duplication (`dryer`), and targeted mutation checks (`mutator`) matched to the repository's setup.
* Reports `PASS`, `FAIL`, `NOT_RUN`, or `N/A` per gate so missing coverage or unrun checks never pass silently.

## Read-Only CLI Flags

`SKILL.md` specifies non-mutating flags so verification leaves `.metrics/` and `git status` clean.

| Gate | Tool | Configured Read-Only Flags in `SKILL.md` |
| --- | --- | --- |
| Complexity | [`crapper`](../../crates/crapper) | `--no-metrics --json`, scoped with `--changed` or explicit paths (`--use-existing-coverage` when fresh coverage is on disk) |
| Duplication | [`dryer`](../../crates/dryer) | `--no-metrics --json --threshold 0.8` across the relevant source root, focusing findings on changed files |
| Mutation sites | [`mutator`](../../crates/mutator) | `--scan --json` first to filter out non-decision lines, then `--lines`, scoped `--test-command` for slow runners, and `--json` (streams live progress on `stderr`, writes `.metrics/mutate/*.edn`) |

## Installation

Copy `skills/ub-quality-gate` into your agent's skills directory (for example `~/.claude/skills/ub-quality-gate/`, `~/.codex/skills/ub-quality-gate/`, `~/.cursor/skills/ub-quality-gate/`, `~/.gemini/skills/ub-quality-gate/`, or `~/.grok/skills/ub-quality-gate/`), then invoke `/ub-quality-gate`.

```sh
cp -r skills/ub-quality-gate ~/.claude/skills/
```

```powershell
Copy-Item -Recurse skills\ub-quality-gate "$HOME\.claude\skills\"
```

## License

Licensed under `0BSD` in [../../LICENSE](../../LICENSE).
