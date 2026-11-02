use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use serde_json::value::RawValue;

pub(crate) const MUTATING_TOOLS: [&str; 2] = ["edit_file", "write_file"];
const COMPILE_MARKERS: &[&str] = &[
    "check",
    "build",
    "compile",
    "tsc",
    "typecheck",
    "mypy",
    "fmt",
    "audit",
];

const LINT_MARKERS: &[&str] = &[
    "clippy", "lint", "eslint", "ruff", "flake8", "rubocop", "vet",
];

const TEST_MARKERS: &[&str] = &[
    "test", "pytest", "jest", "vitest", "rspec", "phpunit", "gradle", "mvn", "verify",
];

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "rb", "php", "c", "cc", "cpp", "h", "hpp",
    "cs", "swift", "kt", "scala", "sh",
];

/// Nothing runs these. The nudge itself tells the model to say so, costing a turn.
const DOC_EXTENSIONS: &[&str] = &["md", "markdown", "rst", "adoc", "txt"];

/// Checkable only by a project's own tooling, so exempt outside one.
const STATIC_WEB_EXTENSIONS: &[&str] = &["html", "htm", "css", "svg"];

const PROJECT_MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Makefile",
    "CMakeLists.txt",
    "composer.json",
    "Gemfile",
    "deno.json",
    "tsconfig.json",
];

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

fn classify_command(command: &str, background: bool) -> Option<CheckKind> {
    if background {
        return None;
    }

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
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if READ_ONLY_COMMANDS.contains(&first) {
            continue;
        }
        if TEST_MARKERS.iter().any(|m| segment.contains(m)) {
            return Some(CheckKind::Tested);
        }
        if LINT_MARKERS.iter().any(|m| segment.contains(m)) {
            best = Some(CheckKind::Linted);
        } else if COMPILE_MARKERS.iter().any(|m| segment.contains(m)) && best.is_none() {
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
    /// The project's linter accepted it.
    Linted,
    /// It was actually executed.
    Tested,
}

fn is_source_file(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

fn extension_in(path: &str, list: &[&str]) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| list.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Whether a project manifest sits beside `path` or in a folder above it, up to `root`.
pub(crate) fn belongs_to_a_project(root: &Path, path: &str) -> bool {
    let full = root.join(path);
    let mut dir = full.parent();
    while let Some(d) = dir {
        if PROJECT_MANIFESTS.iter().any(|name| d.join(name).is_file()) {
            return true;
        }
        if d == root || !d.starts_with(root) {
            return false;
        }
        dir = d.parent();
    }
    false
}

#[derive(Default)]
pub(crate) struct RunLedger {
    /// Workspace-relative paths successfully mutated this run.
    changed: BTreeSet<String>,
    created_source: BTreeSet<String>,
    compiled: bool,
    linted: bool,
    tested: bool,
}

/// What the loop should say, if anything, when a run tries to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nudge {
    /// Files changed and nothing whatsoever was run.
    NothingRan { changed: usize },
    /// New source files were written and only compiled, never executed.
    NewCodeNeverRun { created: usize },
    /// New source files were written and tested, but the project's linter
    /// never ran over them.
    NewCodeNotLinted { created: usize },
}

impl RunLedger {
    pub(crate) fn clear(&mut self) {
        self.changed.clear();
        self.created_source.clear();
        self.clear_checks();
    }

    fn clear_checks(&mut self) {
        self.compiled = false;
        self.linted = false;
        self.tested = false;
    }

    /// Whether anything at all has been run since the last mutation.
    fn anything_ran(&self) -> bool {
        self.compiled || self.linted || self.tested
    }
    pub(crate) fn record(&mut self, tool: &str, args: &RawValue, failed: bool, created: &[String]) {
        if MUTATING_TOOLS.contains(&tool) {
            if failed {
                return;
            }
            if let Ok(parsed) = serde_json::from_str::<PathOnly>(args.get()) {
                self.changed.insert(parsed.path);
                self.clear_checks();
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
            match kind {
                CheckKind::Compiled => self.compiled = true,
                CheckKind::Linted => self.linted = true,
                CheckKind::Tested => self.tested = true,
            }
        }
    }

    pub(crate) fn unchecked_change_count(&self) -> Option<usize> {
        (!self.changed.is_empty() && !self.anything_ran()).then_some(self.changed.len())
    }

    fn nothing_could_check(&self, in_a_project: &impl Fn(&str) -> bool) -> bool {
        self.changed.iter().all(|path| {
            extension_in(path, DOC_EXTENSIONS)
                || (extension_in(path, STATIC_WEB_EXTENSIONS) && !in_a_project(path))
        })
    }

    pub(crate) fn nudge_now(
        &self,
        already_nudged: bool,
        in_a_project: impl Fn(&str) -> bool,
    ) -> Option<Nudge> {
        if already_nudged {
            return None;
        }
        if let Some(changed) = self.unchecked_change_count() {
            if self.nothing_could_check(&in_a_project) {
                return None;
            }
            return Some(Nudge::NothingRan { changed });
        }

        if self.created_source.is_empty() {
            return None;
        }
        if !self.tested {
            return Some(Nudge::NewCodeNeverRun {
                created: self.created_source.len(),
            });
        }

        if !self.linted {
            return Some(Nudge::NewCodeNotLinted {
                created: self.created_source.len(),
            });
        }
        None
    }
}

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

pub(crate) const NEW_CODE_NOT_LINTED_NUDGE: &str = "\
<harness-note>
You wrote new code and ran its tests, but the project's linter has not seen
it. Many projects gate on the linter separately -- `cargo clippy -- -D
warnings`, `eslint --max-warnings 0`, `ruff check` -- and reject code that
compiles and passes every test.
- Run the linter this project uses, over what you changed.
- Fix what it reports and re-run it.
- If this project genuinely has no linter configured, say so in one sentence
  and finish.
</harness-note>";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    fn in_project(_: &str) -> bool {
        true
    }

    fn loose(_: &str) -> bool {
        false
    }

    #[test]
    fn docs_alone_are_never_asked_for_a_check() {
        let mut l = RunLedger::default();
        edit(&mut l, "README.md");
        edit(&mut l, "docs/notes.TXT");
        assert_eq!(l.unchecked_change_count(), Some(2));
        assert_eq!(l.nudge_now(false, in_project), None);
        assert_eq!(l.nudge_now(false, loose), None);
    }

    #[test]
    fn a_loose_web_page_is_not_asked_for_a_check_but_one_in_a_project_is() {
        let mut l = RunLedger::default();
        edit(&mut l, "shooting_game.html");
        edit(&mut l, "style.css");
        assert_eq!(l.nudge_now(false, loose), None);
        assert_eq!(
            l.nudge_now(false, in_project),
            Some(Nudge::NothingRan { changed: 2 })
        );
    }

    #[test]
    fn any_code_among_the_changes_still_gets_the_note() {
        let mut l = RunLedger::default();
        edit(&mut l, "index.html");
        edit(&mut l, "README.md");
        edit(&mut l, "server.js");
        assert_eq!(
            l.nudge_now(false, loose),
            Some(Nudge::NothingRan { changed: 3 })
        );
        let mut config = RunLedger::default();
        edit(&mut config, "package.json");
        assert_eq!(
            config.nudge_now(false, loose),
            Some(Nudge::NothingRan { changed: 1 })
        );
    }

    #[test]
    fn a_project_is_found_beside_the_file_or_above_it_but_not_outside_the_workspace() {
        let root = std::env::temp_dir().join(format!("hm-validation-{}", std::process::id()));
        let app = root.join("apps").join("site");
        std::fs::create_dir_all(app.join("pages")).unwrap();
        assert!(!belongs_to_a_project(&root, "apps/site/pages/index.html"));
        std::fs::write(app.join("package.json"), "{}").unwrap();
        assert!(belongs_to_a_project(&root, "apps/site/pages/index.html"));
        assert!(belongs_to_a_project(&root, "apps/site/index.html"));
        assert!(!belongs_to_a_project(&root, "other/index.html"));
        assert!(!belongs_to_a_project(&root, "index.html"));
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        assert!(belongs_to_a_project(&root, "index.html"));
        let _ = std::fs::remove_dir_all(&root);
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
        assert_eq!(
            l.nudge_now(false, in_project),
            Some(Nudge::NothingRan { changed: 1 })
        );

        // Turn 2: it edits more and stops again, still without checking.
        // The ledger still says the work is unchecked -- and the run is
        // still allowed to end.
        edit(&mut l, "src/other.rs");
        assert_eq!(l.unchecked_change_count(), Some(2));
        assert_eq!(l.nudge_now(true, in_project), None);
    }

    /// A run that takes the note seriously must not then be nudged for the
    /// edits it made *while* fixing things, since it is clearly checking.
    #[test]
    fn a_run_that_checks_after_being_asked_is_not_asked_again() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/main.rs");
        assert_eq!(
            l.nudge_now(false, in_project),
            Some(Nudge::NothingRan { changed: 1 })
        );
        edit(&mut l, "src/main.rs");
        shell(&mut l, "cargo test");
        assert_eq!(l.nudge_now(true, in_project), None);
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
            l.nudge_now(false, in_project),
            Some(Nudge::NewCodeNeverRun { created: 1 }),
            "cargo check must not stand in for running the code"
        );
    }

    #[test]
    fn new_code_that_was_tested_and_linted_is_left_alone() {
        let mut l = RunLedger::default();
        create(&mut l, "src/trace.rs");
        shell(&mut l, "cargo test -p harness-agent");
        shell(&mut l, "cargo clippy --workspace -- -D warnings");
        assert_eq!(l.nudge_now(false, in_project), None);
    }

    /// The real gap: a generated implementation ran `cargo check` and
    /// `cargo test`, never `cargo clippy`, and left two
    /// `unwrap`-after-`is_some` errors in a repository that gates on
    /// `-D warnings`. Passing tests are not evidence about lint.
    #[test]
    fn new_code_that_was_tested_but_never_linted_is_asked_for_the_linter() {
        let mut l = RunLedger::default();
        create(&mut l, "crates/harness-agent/src/latency_trace.rs");
        shell(&mut l, "cd HiveMind && cargo check 2>&1");
        shell(
            &mut l,
            "cd HiveMind && cargo test --package harness-agent 2>&1",
        );
        assert_eq!(
            l.nudge_now(false, in_project),
            Some(Nudge::NewCodeNotLinted { created: 1 })
        );
    }

    /// Lint is recognised on its own -- `cargo clippy` contains neither a
    /// compile nor a test marker.
    #[test]
    fn the_linter_is_recognised_across_ecosystems() {
        for cmd in [
            "cargo clippy -- -D warnings",
            "npx eslint src --max-warnings 0",
            "ruff check .",
            "go vet ./...",
        ] {
            assert_eq!(
                classify_command(cmd, false),
                Some(CheckKind::Linted),
                "{cmd} should read as a lint"
            );
        }
    }

    /// Editing existing code still only needs *something* to have run --
    /// the lint rule is scoped to newly created source, or it would nag on
    /// every one-line fix.
    #[test]
    fn editing_existing_code_is_not_asked_for_the_linter() {
        let mut l = RunLedger::default();
        edit(&mut l, "src/agent.rs");
        shell(&mut l, "cargo test");
        assert_eq!(l.nudge_now(false, in_project), None);
    }

    #[test]
    fn a_compile_after_a_test_does_not_un_run_the_test() {
        let mut l = RunLedger::default();
        create(&mut l, "src/trace.rs");
        shell(&mut l, "cargo test");
        shell(&mut l, "cargo clippy");
        shell(&mut l, "cargo fmt");
        assert_eq!(l.nudge_now(false, in_project), None);
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
        assert_eq!(l.nudge_now(false, in_project), None);
    }

    /// A new README has nothing to execute. Demanding a test for one is the
    /// false alarm that teaches people to ignore the real ones.
    #[test]
    fn a_created_non_source_file_never_demands_a_test() {
        let mut l = RunLedger::default();
        create(&mut l, "docs/DESIGN.md");
        shell(&mut l, "cargo check");
        assert_eq!(l.nudge_now(false, in_project), None);
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
