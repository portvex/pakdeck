# pakdeck: working agreement

## Review loop: no change is done until all reviewers approve

Three reviewer subagents (read-only, in `.claude/agents/`) review every
change. The developer (the main session) never reviews its own work.

| Reviewer | Agent | Looks at |
|---|---|---|
| Software Architect (TUI/Rust) | `software-architect` | module boundaries, Backend/Multi design, state and concurrency design, Rust API design, config and CLI design |
| Senior Rust Code Reviewer | `rust-code-reviewer` | line-level correctness, no-panic policy, command safety, parsers, concurrency, tests |
| UI/UX TUI Designer | `tui-ux-designer` | discoverability, consistency, feedback, layout at 80x24 and large sizes, themes, wording, docs |

After making changes:

1. Run the checks below, then start all three reviewers on the pending
   changes in parallel. Tell them what changed and why, and which checks
   passed. The UI/UX designer may be skipped only for changes nobody can
   see (internal refactors, tests, build scripts); say so in the report.
2. Fix every BLOCKER, MAJOR and MINOR finding. NITs: fix where cheap.
   If a finding is wrong, do not ignore it silently: explain why in the
   re-review request; it stays open until that reviewer accepts the
   reasoning.
3. Rerun the checks, then send the fixes back to **the same reviewer
   instances** (SendMessage, so they keep their context) with a list of
   what was done for each finding. Every reviewer re-reviews, including
   those that already approved, because fixes can cause regressions in
   their area.
4. Repeat steps 2 and 3 until **all three** end with `VERDICT: APPROVE`
   in the same round (no BLOCKER, MAJOR or MINOR finding open).
5. Only then report the work as done: list what changed, the checks and
   integration tests that ran on the final code, and any NITs left open.
   Commit only after that, and only when a commit was asked for.

Never report work as done, and never commit, with a `CHANGES REQUESTED`
open. If developer and a reviewer still disagree after three rounds on
the same finding, stop and ask the user to decide.

## Checks to run before review

- `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `scripts/cross-check.sh` (clippy for Linux, FreeBSD and Windows targets)
- For backend or command changes: `tests/docker/run.sh <distros>`; for
  flatpak/snap/multi-backend: `tests/vm/ubuntu.sh`; for pkg:
  `tests/vm/freebsd.sh`
- For UI/input or parser changes: `scripts/fuzz.sh 60 <target>`

## Code rules

- No panics: the lints in `Cargo.toml` forbid `unwrap`, `expect`,
  indexing, slicing and unchecked arithmetic outside tests. Also avoid
  `clamp` with possibly crossed bounds, `thread::spawn` (use
  `thread::Builder`), `println!` (use the `out()` helper), `env::args`.
- Package-manager specifics live in their backend module behind the
  `Backend` trait; report missing capabilities with `unsupported(...)`.
- Every parser gets a unit test with realistic output.
