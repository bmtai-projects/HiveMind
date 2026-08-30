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

/// Substrings that mean a shell command *compiled or linted* the work.
///
/// These prove the code parses and type-checks. They prove nothing about
/// whether it behaves, which is a different claim and used to be treated as
/// the same one -- `"check"` sat in the same list as `"test"`, so a run that
/// only ever executed `cargo check` was recorded as verified. That is not a
/// hypothetical: a measured run created a new module, ran `cargo check`
/// twice, never ran a single test, and shipped a bug that made 7 of its 9
/// features silently stop working after the first request. The harness
/// agreed the work had been checked.
const COMPILE_MARKERS: &[&str] = &[
    "check",
    "lint",
    "build",
    "compile",
    "clippy",
    "tsc",
    "typecheck",
    "mypy",
    "fmt",
    "vet",
    "audit",
    "eslint",
    "ruff",
    "flake8",
    "rubocop",
];

/// Substrings that mean a shell command actually *ran* the work.
///
/// Matching stays deliberately generous, for the same asymmetry as before: a
/// false positive costs nothing (no nudge, which is what shipped for every
/// release before this), a false negative costs one wasted turn.
const TEST_MARKERS: &[&str] = &[
    "test", "pytest", "jest", "vitest", "rspec", "phpunit", "gradle", "mvn", "verify",
];

/// Extensions that count as source code for the "new code, never run" rule.
///
/// A newly created `.md` or `.toml` has nothing to execute, and demanding a
/// test for one would be the kind of false alarm that teaches people to
/// ignore the real ones.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "rb", "php", "c", "cc", "cpp", "h", "hpp",
    "cs", "swift", "kt", "scala", "sh",
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
fn classify_command(command: &str, background: bool) -> Option<CheckKind> {
    if background {
        return None;
    }
    // Judged per segment, not on the whole string. Agents overwhelmingly
    // write `cd repo && cargo check 2>&1 | head -80`, and looking only at
    // the first token of that sees `cd` -- a read-only command -- and
    // concludes nothing was checked at all. The measured run that motivated
    // this module ran exactly two shell commands, and *both* began `cd
    // HiveMind && ...`, so the original first-token rule scored a run that
    // compiled twice as having run nothing.
    let lowered = command.trim().to_lowercase();
    let normalized = lowered
        .replace("&&", ";")
        .replace("||", ";")
        .replace('|', ";");

    let mut best: Option<CheckKind> = None;
    for segment in normalized.split(';') {
        let segment = segment.trim();
        let first = segment
            .split_whitespace()
            .next()
            .unwrap_or_default()
            // `./scripts/test.sh` and `/usr/bin/ls` should be judged on the
            // program name, not the path that reached it.
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if READ_ONLY_COMMANDS.contains(&first) {
            continue;
        }
        // Tested wins wherever it appears: actually running the code is the
        // stronger claim, and the whole point of the split is not to let the
        // weaker one stand in for it.
        if TEST_MARKERS.iter().any(|m| segment.contains(m)) {
            return Some(CheckKind::Tested);
        }
        if COMPILE_MARKERS.iter().any(|m| segment.contains(m)) {
            best = Some(CheckKind::Compiled);
        }
    }
    best
}

/// How strong a claim one shell command makes about the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckKind {
    /// It parses and type-checks.
    Compiled,
    /// It was actually executed.
    Tested,
}

fn is_source_file(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// What one run has done to the workspace, and how hard anyone has looked
/// at it since.
///
/// Scoped to a single `run()` -- one user request -- and reset at its top.
/// A check that ran in answer to an earlier request says nothing about files
/// changed in this one.
#[derive(Default)]
pub(crate) struct RunLedger {
    /// Workspace-relative paths successfully mutated this run.
    changed: BTreeSet<String>,
    /// Paths *created* this run that look like source code. Tracked apart
    /// from `changed` because new code carries a claim an edit does not:
    /// nothing has ever executed it, so "it compiles" is a much weaker
    /// statement about it than about a two-line change to a tested file.
    created_source: BTreeSet<String>,
    /// The strongest claim made since the last mutation. `None` means
    /// nothing has been run at all. Reset by every mutation, so ordering is
    /// what it measures -- testing and *then* editing leaves the edit
    /// unchecked, which a plain "did a test run this turn?" flag waves
    /// through.
    checked_since_last_change: Option<CheckKind>,
}

/// What the loop should say, if anything, when a run tries to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nudge {
    /// Files changed and nothing whatsoever was run.
    NothingRan { changed: usize },
    /// New source files were written and only compiled, never executed.
    NewCodeNeverRun { created: usize },
}

impl RunLedger {
    pub(crate) fn clear(&mut self) {
        self.changed.clear();
        self.created_source.clear();
        self.checked_since_last_change = None;
    }

    /// Folds one completed tool call into the ledger.
    ///
    /// `created` carries the paths the tool reported creating, taken from
    /// `ToolResult::changed_files` rather than guessed from arguments --
    /// `write_file` already reports `FileChangeKind::Created`, and that is
    /// the only way to tell a brand-new module from a rewrite of an existing
    /// one.
    ///
    /// `failed` is used differently for the two cases on purpose. A failed
    /// `edit_file` wrote nothing, so it is not a change to answer for. A
    /// failed `cargo test` is still a test run -- arguably the most valuable
    /// kind -- so it counts regardless: the model looked, and what it saw is
    /// now in its context.
    pub(crate) fn record(&mut self, tool: &str, args: &RawValue, failed: bool, created: &[String]) {
        if MUTATING_TOOLS.contains(&tool) {
            if failed {
                return;
            }
            if let Ok(parsed) = serde_json::from_str::<PathOnly>(args.get()) {
                self.changed.insert(parsed.path);
                self.checked_since_last_change = None;
            }
            for path in created {
                if is_source_file(path) {
                    self.created_source.insert(path.clone());
                }
            }
            return;
        }
        if tool == "run_shell"
            && let Ok(parsed) = serde_json::from_str::<ShellOnly>(args.get())
            && let Some(kind) = classify_command(&parsed.command, parsed.background)
        {
            // Never downgrade: a `cargo check` after a `cargo test` does not
            // un-run the tests.
            if self.checked_since_last_change != Some(CheckKind::Tested) {
                self.checked_since_last_change = Some(kind);
            }
        }
    }

    /// How many files are changed and entirely unchecked, or `None` when the
    /// run has nothing to answer for on that count.
    pub(crate) fn unchecked_change_count(&self) -> Option<usize> {
        (!self.changed.is_empty() && self.checked_since_last_change.is_none())
            .then_some(self.changed.len())
    }

    /// The whole policy, in one place: what to say when a run tries to end,
    /// or `None` to let it finish.
    ///
    /// Kept here rather than inline at the call site so the part with a
    /// decision in it -- *including* the one-shot rule, which is the half
    /// that makes this safe -- is testable without standing up a provider to
    /// drive the loop.
    pub(crate) fn nudge_now(&self, already_nudged: bool) -> Option<Nudge> {
        if already_nudged {
            return None;
        }
        if let Some(changed) = self.unchecked_change_count() {
            return Some(Nudge::NothingRan { changed });
        }
        // New code that was compiled but never executed. Compiling proves it
        // parses; it proves nothing about the behaviour the run was asked
        // for, and this is exactly the gap a measured run fell through --
        // `cargo check` twice, no tests, a bug that disabled 7 of 9 features
        // after the first request.
        if self.checked_since_last_change == Some(CheckKind::Compiled)
            && !self.created_source.is_empty()
        {
            return Some(Nudge::NewCodeNeverRun {
                created: self.created_source.len(),
            });
        }
        None
    }
}

/// Delivered once per run, when a run that changed files tries to end
/// without running anything at all.
///
/// Written the same way as `STALL_NUDGE`: as instructions rather than an
/// accusation, and with an explicit way out. The escape hatch in the last
/// line is not politeness -- it is what keeps this from being a trap.
pub(crate) const NOTHING_RAN_NUDGE: &str = "\
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

/// Delivered once per run, when new source files were written and only
/// compiled -- never executed.
///
/// Deliberately *not* offering the "if nothing applies, say so" escape the
/// note above offers. That out exists for a repo with nothing to run; this
/// note only fires when the run has already successfully run a compile step,
/// which means a toolchain exists and the same toolchain can run tests. The
/// honest answer here is almost never "nothing applies", so offering it
/// would mostly be offering a way to skip the work.
///
/// It asks for one specific thing -- exercise it twice -- because that is
/// what the failure it was built from needed. The module in question latched
/// its state on the first call and went silent for every request after it;
/// any test that invoked it a second time would have failed instantly, and
/// no test that invoked it once ever could.
pub(crate) const NEW_CODE_NEVER_RUN_NUDGE: &str = "\
<harness-note>
You wrote new code and compiled it, but nothing has executed it. Compiling
proves it parses; it says nothing about whether it does what was asked.
Before finishing:
- Add a test for the new code and run it. Cover the behaviour the task
  actually asked for, not just that it constructs.
- If it holds state across calls, exercise it more than once. State that
  latches on the first call and silently does nothing afterwards is the
  single most common way this kind of code ships broken.
- If a test fails, fix it and re-run. Reporting a failure you introduced is
  not finishing.
</harness-note>";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    fn edit(ledger: &mut RunLedger, path: &str) {
        ledger.record(
            "edit_file",
            &args(serde_json::json!({"path": path})),
            false,
            &[],
        );
    }

    fn shell(ledger: &mut RunLedger, command: &str) {
        ledger.record(
            "run_shell",
            &args(serde_json::json!({"command": command})),
            false,
            &[],
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
            &[],
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
            &[],
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
            &[],
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
                classify_command(cmd, false).is_some(),
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
                !classify_command(cmd, false).is_some(),
                "{cmd} should not count as validation"
            );
        }
    }

    /// A backgrounded dev server produces no result to judge, and its
    /// command line is exactly the kind of string that matches a marker.
    #[test]
    fn a_backgrounded_command_never_counts_however_much_it_looks_like_a_check() {
        assert!(!classify_command("npm run build:watch", true).is_some());
        assert!(!classify_command("cargo watch -x test", true).is_some());
        assert!(classify_command("npm run build:watch", false).is_some());
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
        assert_eq!(l.nudge_now(false), Some(Nudge::NothingRan { changed: 1 }));

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
        assert_eq!(l.nudge_now(false), Some(Nudge::NothingRan { changed: 1 }));
        edit(&mut l, "src/main.rs");
        shell(&mut l, "cargo test");
        assert_eq!(l.nudge_now(true), None);
        assert_eq!(l.unchecked_change_count(), None);
    }

    fn create(ledger: &mut RunLedger, path: &str) {
        ledger.record(
            "write_file",
            &args(serde_json::json!({ "path": path })),
            false,
            &[path.to_string()],
        );
    }

    /// The real shell commands from the measured run. Both begin `cd
    /// HiveMind && ...`, and a first-token rule reads `cd`, calls the whole
    /// thing read-only, and scores a run that compiled twice as having run
    /// nothing at all.
    #[test]
    fn a_compound_command_is_judged_by_its_parts_not_its_first_word() {
        assert_eq!(
            classify_command("cd HiveMind && cargo check 2>&1 | head -80", false),
            Some(CheckKind::Compiled)
        );
        assert_eq!(
            classify_command(
                "cd HiveMind && cargo fmt && cargo check 2>&1 | tail -5",
                false
            ),
            Some(CheckKind::Compiled)
        );
        assert_eq!(
            classify_command("cd repo && cargo test --workspace", false),
            Some(CheckKind::Tested)
        );
        // Still not fooled by a read-only command with a check-ish argument.
        assert_eq!(classify_command("cat test_fixtures.json", false), None);
        assert_eq!(classify_command("cd repo && ls build/", false), None);
    }

    /// The exact run this rule was built from: a new module written,
    /// `cargo check` run twice, no test ever executed. The old single-class
    /// marker list called that validated, and the bug it hid disabled 7 of
    /// the feature's 9 stages after the first request.
    #[test]
    fn new_code_that_was_only_compiled_is_still_asked_for_a_test() {
        let mut l = RunLedger::default();
        create(&mut l, "crates/harness-agent/src/trace.rs");
        shell(&mut l, "cd HiveMind && cargo check 2>&1 | head -80");
        shell(
            &mut l,
            "cd HiveMind && cargo fmt && cargo check 2>&1 | tail -5",
        );

        assert_eq!(
            l.nudge_now(false),
            Some(Nudge::NewCodeNeverRun { created: 1 }),
            "cargo check must not stand in for running the code"
        );
    }

    #[test]
    fn new_code_that_was_actually_tested_is_left_alone() {
        let mut l = RunLedger::default();
        create(&mut l, "src/trace.rs");
        shell(&mut l, "cargo test -p harness-agent");
        assert_eq!(l.nudge_now(false), None);
    }

    /// Tested is the stronger claim and must not be undone by a later
    /// compile-only command.
    #[test]
    fn a_compile_after_a_test_does_not_un_run_the_test() {
        let mut l = RunLedger::default();
        create(&mut l, "src/trace.rs");
        shell(&mut l, "cargo test");
        shell(&mut l, "cargo fmt");
        assert_eq!(l.nudge_now(false), None);
    }

    /// One command that does both counts as the stronger of the two --
    /// `cargo fmt && cargo test` is a test run.
    #[test]
    fn a_combined_command_is_judged_by_its_strongest_claim() {
        assert_eq!(
            classify_command("cargo fmt && cargo test", false),
            Some(CheckKind::Tested)
        );
        assert_eq!(
            classify_command("cargo check", false),
            Some(CheckKind::Compiled)
        );
    }

    /// Editing existing, already-tested code and compiling it is a much
    /// weaker claim than writing new code -- this rule deliberately does not
    /// fire there, or it would nag on every small fix.
    #[test]
    fn editing_existing_code_and_compiling_it_is_not_nagged() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/agent.rs");
        shell(&mut l, "cargo check");
        assert_eq!(l.nudge_now(false), None);
    }

    /// A new README has nothing to execute. Demanding a test for one is the
    /// false alarm that teaches people to ignore the real ones.
    #[test]
    fn a_created_non_source_file_never_demands_a_test() {
        let mut l = RunLedger::default();
        create(&mut l, "docs/DESIGN.md");
        shell(&mut l, "cargo check");
        assert_eq!(l.nudge_now(false), None);
    }

    /// The stronger note must not hand back the "nothing applies" out: it
    /// only fires when a compile has already succeeded, so a toolchain
    /// demonstrably exists and could run tests too.
    #[test]
    fn the_new_code_note_does_not_offer_an_escape_hatch() {
        assert!(NOTHING_RAN_NUDGE.contains("If nothing applies"));
        assert!(!NEW_CODE_NEVER_RUN_NUDGE.contains("If nothing applies"));
        assert!(NEW_CODE_NEVER_RUN_NUDGE.contains("exercise it more than once"));
    }

    #[test]
    fn the_nudge_names_the_escape_hatch_so_it_cannot_become_a_trap() {
        assert!(NOTHING_RAN_NUDGE.contains("If nothing applies"));
        assert!(NOTHING_RAN_NUDGE.contains("Do not invent a command"));
    }
}
