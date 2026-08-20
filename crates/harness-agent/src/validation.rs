//! One question, asked once per run: *you changed files — did you check
//! that they work?*
//!
//! A run ends the moment the model answers without calling a tool
//! (`agent.rs`'s `if !has_tool_calls`). Nothing today distinguishes "I read
//! the code and here's your answer" from "I rewrote four files and I'm
//! declaring victory without compiling any of them." Both simply end the
//! turn. This module is the difference: it tracks, per run, whether the
//! workspace was actually mutated and whether anything has been run to
//! check it *since* the last mutation, so the loop can push back exactly
//! once before letting the run finish.
//!
//! # Why a nudge and not a hard gate
//!
//! The obvious design -- refuse to complete until validation happens -- is
//! the wrong one here, and expensively so. The harness cannot know what
//! "validated" means in an arbitrary repository, so a hard gate has a
//! failure mode with no floor: a model that doesn't know how to satisfy it
//! keeps trying until `max_turns`, burning a full run's budget to produce
//! nothing. That is strictly worse than the premature completion it was
//! meant to prevent. So this fires **once** per run and then gets out of
//! the way -- the same shape, for the same reason, as the existing
//! `STALL_NUDGE`/`nudged_this_run` pair in `agent.rs`. Worst case is one
//! extra turn, bounded and paid once.
//!
//! # What this deliberately does not claim
//!
//! It does not check that validation *passed*. A model that runs the tests,
//! sees three failures, and declares success anyway is a real problem, and
//! not this one -- catching it means understanding arbitrary tool output
//! well enough to grade it. What is checked here is narrower and fully
//! decidable from the harness's own records: whether the model *looked*.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::value::RawValue;

/// The tools whose success means the workspace is now different from what
/// the model last saw checked.
///
/// Shared with `checkpoint.rs` rather than duplicated, because the two
/// features have to agree: a tool that mutates the workspace needs both a
/// pre-turn snapshot to undo to *and* a check before the run ends, and a
/// future tool added to one list but not the other would silently get one
/// half of that.
///
/// `run_shell` is not here, and can't be. A shell command's effect on the
/// workspace is unbounded and invisible from the call site -- `ls` and a
/// codemod are the same shape from here -- so treating every shell call as
/// a mutation would make the most common read-only command in the tool set
/// demand validation of nothing. The cost of that omission is real and
/// worth stating plainly: work done entirely through `run_shell` is not
/// tracked as a change by this module, exactly as it is not covered by
/// `/undo`.
pub(crate) const MUTATING_TOOLS: [&str; 2] = ["edit_file", "write_file"];

/// Substrings that mean a shell command was *checking* the work rather than
/// doing more of it. Matched against the whole command, case-insensitively.
///
/// Matching is deliberately generous, because the two errors are not
/// symmetric. A false positive here (some command containing "test" that
/// wasn't really a check) costs exactly nothing -- no nudge fires, which is
/// the behaviour that shipped for every release before this one. A false
/// negative costs one unnecessary nudge in a run that was already correct,
/// which is mildly annoying and burns a turn. When in doubt, count it.
const VALIDATION_MARKERS: &[&str] = &[
    "test",
    "check",
    "lint",
    "build",
    "compile",
    "clippy",
    "tsc",
    "typecheck",
    "mypy",
    "pytest",
    "jest",
    "vitest",
    "eslint",
    "ruff",
    "flake8",
    "rubocop",
    "rspec",
    "phpunit",
    "gradle",
    "mvn",
    "make",
    "vet",
    "fmt",
    "audit",
    "verify",
];

/// First tokens that mean "I am reading, not checking" -- these never count
/// as validation however much they look like it. Without this, `cat
/// test_fixtures.json` and `rm -rf build/` both read as a passing check.
const READ_ONLY_COMMANDS: &[&str] = &[
    "cat", "ls", "head", "tail", "grep", "rg", "find", "cd", "echo", "pwd", "which", "wc", "less",
    "more", "tree", "stat", "file", "rm", "mv", "cp", "mkdir", "touch",
];

#[derive(Deserialize)]
struct PathOnly {
    path: String,
}

#[derive(Deserialize)]
struct ShellOnly {
    command: String,
    #[serde(default)]
    background: bool,
}

/// Whether a shell command counts as having checked the work.
///
/// Background commands never do: `run_shell` with `background: true`
/// returns before the command has produced a result, so nothing about its
/// outcome is known at the point it would satisfy the check -- and the
/// thing most often started that way is a dev server, whose command line
/// ("npm run build:watch") is exactly the kind of string that would
/// otherwise match.
fn looks_like_validation(command: &str, background: bool) -> bool {
    if background {
        return false;
    }
    let lowered = command.trim().to_lowercase();
    let first = lowered
        .split_whitespace()
        .next()
        .unwrap_or_default()
        // `./scripts/test.sh` and `/usr/bin/ls` should be judged on the
        // program name, not the path that reached it.
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if READ_ONLY_COMMANDS.contains(&first) {
        return false;
    }
    VALIDATION_MARKERS.iter().any(|m| lowered.contains(m))
}

/// What one run has done to the workspace, and whether it has looked since.
///
/// Scoped to a single `run()` -- one user request -- and reset at its top,
/// not carried across the session. A check that ran in answer to an earlier
/// request says nothing about files changed in this one.
#[derive(Default)]
pub(crate) struct RunLedger {
    /// Workspace-relative paths successfully mutated this run. A set, not a
    /// count: rewriting the same file six times is one file's worth of
    /// unchecked change, and the count is what the nudge reports.
    changed: BTreeSet<String>,
    /// Cleared by every mutation, set by every validating shell command --
    /// so ordering is what it measures. Running the tests and *then*
    /// editing leaves the edit unchecked, which is the case a plain
    /// "did a test run this turn?" flag would wave through.
    checked_since_last_change: bool,
}

impl RunLedger {
    pub(crate) fn clear(&mut self) {
        self.changed.clear();
        self.checked_since_last_change = false;
    }

    /// Folds one completed tool call into the ledger.
    ///
    /// `failed` is the tool's own reported status, and the two cases use it
    /// differently on purpose. A failed `edit_file` wrote nothing, so it is
    /// not a change to answer for. A failed `cargo test` is still a check --
    /// arguably the most valuable kind -- so it counts regardless: the model
    /// looked, and what it saw was a failure it now has in context.
    pub(crate) fn record(&mut self, tool: &str, args: &RawValue, failed: bool) {
        if MUTATING_TOOLS.contains(&tool) {
            if failed {
                return;
            }
            if let Ok(parsed) = serde_json::from_str::<PathOnly>(args.get()) {
                self.changed.insert(parsed.path);
                self.checked_since_last_change = false;
            }
            return;
        }
        if tool == "run_shell"
            && let Ok(parsed) = serde_json::from_str::<ShellOnly>(args.get())
            && looks_like_validation(&parsed.command, parsed.background)
        {
            self.checked_since_last_change = true;
        }
    }

    /// How many files are changed and unchecked right now, or `None` when
    /// the run has nothing to answer for.
    pub(crate) fn unchecked_change_count(&self) -> Option<usize> {
        (!self.changed.is_empty() && !self.checked_since_last_change).then_some(self.changed.len())
    }

    /// The whole policy, in one place: how many changed files to name in
    /// the nudge, or `None` to let the run end.
    ///
    /// Kept here rather than inline at the call site so the part with a
    /// decision in it -- *including* the one-shot rule, which is the half
    /// that makes this safe -- is testable without standing up a provider
    /// to drive the loop.
    pub(crate) fn nudge_now(&self, already_nudged: bool) -> Option<usize> {
        if already_nudged {
            return None;
        }
        self.unchecked_change_count()
    }
}

/// Delivered once per run, when a run that changed files tries to end
/// without checking them.
///
/// Written the same way as `STALL_NUDGE`: as instructions rather than an
/// accusation, and with an explicit way out. The escape hatch in the last
/// line is not politeness -- it is what keeps this from being a trap. Plenty
/// of edits genuinely have nothing to run (a README, a config file in a repo
/// with no test suite), and a model with no way to say so would either
/// invent a command to satisfy the harness or spend the rest of the run
/// trying to.
pub(crate) const VALIDATION_NUDGE: &str = "\
<harness-note>
You changed files in this task but have not run anything that checks them.
Before finishing:
- Run the project's own check -- its tests, type-check, build, or linter,
  whichever actually applies here. Prefer the narrowest one that covers what
  you changed over the full suite.
- If it fails, fix it and re-run. Reporting a failure you introduced is not
  finishing.
- If nothing applies -- documentation, config, a repo with no suite -- say so
  in one sentence and finish. Do not invent a command to satisfy this note.
</harness-note>";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    fn edit(ledger: &mut RunLedger, path: &str) {
        ledger.record("edit_file", &args(serde_json::json!({"path": path})), false);
    }

    fn shell(ledger: &mut RunLedger, command: &str) {
        ledger.record(
            "run_shell",
            &args(serde_json::json!({"command": command})),
            false,
        );
    }

    #[test]
    fn a_run_that_changed_nothing_is_never_asked_to_validate() {
        let mut l = RunLedger::default();
        shell(&mut l, "ls -la");
        l.record(
            "read_file",
            &args(serde_json::json!({"path": "src/main.rs"})),
            false,
        );
        assert_eq!(l.unchecked_change_count(), None);
    }

    #[test]
    fn an_edit_with_no_check_after_it_is_flagged() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        assert_eq!(l.unchecked_change_count(), Some(1));
    }

    #[test]
    fn a_check_after_the_edit_clears_it() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        shell(&mut l, "cargo test");
        assert_eq!(l.unchecked_change_count(), None);
    }

    /// The ordering case a per-turn "did anything get tested?" flag gets
    /// wrong, and the whole reason the flag is cleared by mutation rather
    /// than set once per run.
    #[test]
    fn checking_before_the_last_edit_does_not_count() {
        let mut l = RunLedger::default();
        edit(&mut l, "a.rs");
        shell(&mut l, "cargo test");
        edit(&mut l, "b.rs");
        assert_eq!(l.unchecked_change_count(), Some(2));
    }

    #[test]
    fn the_same_file_edited_repeatedly_counts_once() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        edit(&mut l, "src/main.rs");
        edit(&mut l, "src/main.rs");
        assert_eq!(l.unchecked_change_count(), Some(1));
    }

    #[test]
    fn a_failed_edit_changed_nothing_so_there_is_nothing_to_check() {
        let mut l = RunLedger::default();
        l.record(
            "edit_file",
            &args(serde_json::json!({"path": "src/main.rs"})),
            true,
        );
        assert_eq!(l.unchecked_change_count(), None);
    }

    /// A failing test run is still a test run: the model looked, and the
    /// failure is now in its context to act on.
    #[test]
    fn a_failing_check_still_counts_as_having_looked() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        l.record(
            "run_shell",
            &args(serde_json::json!({"command": "cargo test"})),
            true,
        );
        assert_eq!(l.unchecked_change_count(), None);
    }

    #[test]
    fn the_usual_check_commands_across_ecosystems_are_recognised() {
        for cmd in [
            "cargo test",
            "cargo clippy --workspace -- -D warnings",
            "npm test",
            "npm run build",
            "pnpm run typecheck",
            "npx tsc --noEmit",
            "pytest -q tests/",
            "go test ./...",
            "go vet ./...",
            "make check",
            "./scripts/test.sh",
            "mvn verify",
            "bundle exec rspec",
        ] {
            assert!(
                looks_like_validation(cmd, false),
                "{cmd} should count as validation"
            );
        }
    }

    #[test]
    fn reading_a_file_that_happens_to_be_named_test_is_not_validation() {
        for cmd in [
            "cat test_fixtures.json",
            "ls build/",
            "rm -rf build",
            "grep -r test src/",
            "find . -name '*.test.ts'",
        ] {
            assert!(
                !looks_like_validation(cmd, false),
                "{cmd} should not count as validation"
            );
        }
    }

    /// A backgrounded dev server produces no result to judge, and its
    /// command line is exactly the kind of string that matches a marker.
    #[test]
    fn a_backgrounded_command_never_counts_however_much_it_looks_like_a_check() {
        assert!(!looks_like_validation("npm run build:watch", true));
        assert!(!looks_like_validation("cargo watch -x test", true));
        assert!(looks_like_validation("npm run build:watch", false));
    }

    #[test]
    fn clear_resets_everything_for_the_next_run() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        assert!(l.unchecked_change_count().is_some());
        l.clear();
        assert_eq!(l.unchecked_change_count(), None);
    }

    /// The property the whole design rests on: a run that ignores the note
    /// and answers again is let through. Without this the harness can hold
    /// a run hostage to a check it cannot describe, all the way to
    /// `max_turns` -- which costs a full budget and produces nothing.
    #[test]
    fn the_nudge_fires_once_and_then_never_again_however_stubborn_the_model_is() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");

        // Turn 1: the model stops without checking. Asked once.
        assert_eq!(l.nudge_now(false), Some(1));

        // Turn 2: it edits more and stops again, still without checking.
        // The ledger still says the work is unchecked -- and the run is
        // still allowed to end.
        edit(&mut l, "src/other.rs");
        assert_eq!(l.unchecked_change_count(), Some(2));
        assert_eq!(l.nudge_now(true), None);
    }

    /// A run that takes the note seriously must not then be nudged for the
    /// edits it made *while* fixing things, since it is clearly checking.
    #[test]
    fn a_run_that_checks_after_being_asked_is_not_asked_again() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        assert_eq!(l.nudge_now(false), Some(1));
        edit(&mut l, "src/main.rs");
        shell(&mut l, "cargo test");
        assert_eq!(l.nudge_now(true), None);
        assert_eq!(l.unchecked_change_count(), None);
    }

    #[test]
    fn the_nudge_names_the_escape_hatch_so_it_cannot_become_a_trap() {
        assert!(VALIDATION_NUDGE.contains("If nothing applies"));
        assert!(VALIDATION_NUDGE.contains("Do not invent a command"));
    }
}
