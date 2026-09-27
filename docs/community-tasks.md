# Community tasks

Eight small, ready-to-share tasks for new contributors. Each one is written so
it can be copied into a GitHub issue as it is.

**For maintainers:** to publish a task, copy its block into a new issue. The
heading is the title. Use the suggested labels. Before you share one, check that
nobody has already fixed it, because line numbers drift over time. The function
and file names are the reliable part. Every task was checked against the code
when this file was written.

**For contributors:** pick one, comment on the issue to say you are on it, and
read [CONTRIBUTING.md](../CONTRIBUTING.md) for the steps. All the checks run
without an API key. If anything is unclear, ask in the issue. That is what it
is for.

The times are rough guesses for someone new to the code.

| # | Task | Kind | Rough time |
|---|---|---|---|
| 1 | [Fix the outdated "6 real coding models" text](#1-fix-the-outdated-6-real-coding-models-text) | small bug | 30 to 60 min |
| 2 | [Remove Rust internals from `--help`](#2-remove-rust-internals-from---help) | polish | 30 to 45 min |
| 3 | [Add tests for the startup box](#3-add-tests-for-the-startup-box) | tests | 1 to 2 hours |
| 4 | [Add a `no-sudo` safety preset](#4-add-a-no-sudo-safety-preset) | small feature | 1 to 2 hours |
| 5 | [Add a fifth built-in skill](#5-add-a-fifth-built-in-skill) | writing | 1 to 2 hours |
| 6 | [Add a way to turn off the startup update check](#6-add-a-way-to-turn-off-the-startup-update-check) | small feature | 1 to 2 hours |
| 7 | [Windows: fail clearly on a trailing `&`](#7-windows-fail-clearly-on-a-trailing--in-run_shell) | bug | 2 to 3 hours |
| 8 | [Write "How one turn works"](#8-write-how-one-turn-works) | docs | 2 to 4 hours |

---

## 1. Fix the outdated "6 real coding models" text

**Why it matters.** The box that appears every time you start `hivemind activate`
says "hivemind (cheap default) + 6 real coding models". There are now 7 besides
`hivemind`, so the first thing a new user reads is wrong. The same old number is
written in three more places.

**Start here.**

- `crates/harness-cli/src/banner.rs` (line ~67): the startup box.
- `crates/harness-cli/src/main.rs` (lines ~4, ~247, ~381): one comment and two
  `--help` texts.
- `config.example.toml` (line ~8): a comment.
- `crates/harness-config/src/lib.rs`: `KNOWN_MODELS` is the real list. It holds
  `hivemind` itself plus the others.

**The change.** Stop typing the number by hand. In the banner, work it out from
`KNOWN_MODELS` (one less than its length, because the list includes `hivemind`).
Put that line in a small function that returns a `String`, so a test can call
it. The `--help` texts and the config comment cannot calculate anything, so
reword them without a number, for example "plus other coding models. Run
`hivemind models` to see them."

**Skills and time.** Basic Rust (strings and `format!`). About 30 to 60 minutes.

**Suggested labels.** `good first issue`, `bug`

**Done when.**

- [ ] Searching `crates/` and `config.example.toml` for `6 real`, `six real`,
      `plus 6`, and `one of the 6` finds nothing.
- [ ] The banner's number comes from `KNOWN_MODELS`.
- [ ] A new test fails if the banner's number and the catalog ever disagree.
- [ ] You ran `hivemind activate` and the right edge of the box is still
      straight.
- [ ] The four checks in [CONTRIBUTING.md](../CONTRIBUTING.md#check-it) pass.

**Depends on.** Nothing.

---

## 2. Remove Rust internals from `--help`

**Why it matters.** `hivemind --help` shows text that was written for
developers. It says "see `crate::self_update` for the full explanation..." and
"See `json_ui::JsonUi` for the event shapes and `run_json_protocol` for the
dispatch loop." A user cannot look those up, and it makes the tool feel
unfinished. This happens because the command-line library (`clap`) turns `///`
doc comments into help text.

**Start here.** `crates/harness-cli/src/main.rs`, three doc comments:

- On the `Update` command (line ~256).
- On the `Json` variant of `enum Protocol` (line ~355).
- On the `protocol` field of the activate arguments (line ~367).

They show up on `hivemind --help`, `hivemind update --help`, and
`hivemind activate --help`.

**The change.** For each one, keep a short sentence written for users as the
help text. Move the developer explanation into ordinary `//` comments just above
it, so nobody loses the information.

**Skills and time.** Basic Rust. Knowing `clap` helps but is not needed. About 30
to 45 minutes.

**Suggested labels.** `good first issue`, `enhancement`

**Done when.**

- [ ] The three help screens above no longer contain `crate::`, `json_ui`, or
      `run_json_protocol`.
- [ ] The developer notes are still in the code as `//` comments.
- [ ] The four checks pass.
- [ ] *Optional stretch:* add a test that walks every command and subcommand and
      fails if any help text contains `::`, so this cannot come back.

**Depends on.** Nothing. It edits `main.rs` near task 1, so if both are open,
whoever finishes second may need a quick rebase.

---

## 3. Add tests for the startup box

**Why it matters.** `banner.rs` draws the box shown at every startup, and it has
no tests. The padding maths in `boxed()` is exactly the sort of thing a small
future change can quietly break, and nothing would notice.

**Start here.** `crates/harness-cli/src/banner.rs`: the functions `boxed` and
`boxed_blank`, and the constant `INNER_WIDTH` (58). There is no test module yet,
so add `#[cfg(test)] mod tests { use super::*; ... }` at the bottom. The bottom
of `crates/harness-cli/src/hook_presets.rs` shows the naming style: test names
are sentences.

**The change.** Add tests only. Do not change how the box works. Ideas:

- Empty text gives a line that is 62 characters wide once you ignore colors
  (a border, a space, 58 characters, a space, a border).
- Short text is padded to that same width.
- Bold, dim, and reset color codes are not counted as width.
- Text of exactly 58 characters gets no extra padding.
- Text longer than 58 characters does not panic and is not cut off. This
  records what happens today.
- `boxed_blank()` gives the same result as `boxed("")`.

You will need a small helper inside the test module that removes color codes
(`\x1b[` up to the next `m`) before counting characters.

**Skills and time.** Basic Rust testing. About 1 to 2 hours.

**Suggested labels.** `good first issue`

**Done when.**

- [ ] At least five new tests, and they pass.
- [ ] No code outside `#[cfg(test)]` changed.
- [ ] The four checks pass.
- [ ] If a test reveals a real bug, open a separate issue for it. Do not fix it
      in this pull request.

**Depends on.** Nothing.

---

## 4. Add a `no-sudo` safety preset

**Why it matters.** `hivemind hooks list` offers a few one-command safety rules,
like "never force-push". One obvious rule is missing: do not let the agent
install or change things as the root user with `sudo`. Writing a preset is a
small, self-contained way to learn how hooks work.

**Start here.** `crates/harness-cli/src/hook_presets.rs`:

- `PRESETS` (line ~54): the list of presets.
- `evaluate()` (line ~133): picks which preset function runs.
- `no_force_push()` (line ~166): the closest one to copy. It uses the helper
  `shell_command_text()` to get the command being run.
- The tests at the bottom of the file, which use small helpers named `env()` and
  `deny_reason()`.

No other file needs to change. `hivemind hooks list` reads `PRESETS` on its
own.

**The change.** Add a preset named `no-sudo`. It blocks a `run_shell` command
when `sudo` is the first word of any command in the line. That means the first
word of the whole line, or the first word after `&&`, `||`, `;`, or `|`. Its
message should say what was blocked and suggest running it outside HiveMind.
Like the other presets, it catches the obvious cases and says so. It is not a
sandbox.

Expected behavior:

| Command | Result |
|---|---|
| `sudo apt install ripgrep` | blocked |
| `ls && sudo rm notes.txt` | blocked |
| `echo pseudo` | allowed |
| `sudoku --play` | allowed |
| `echo "use sudo later"` | allowed |
| any tool other than `run_shell` | allowed |

**Skills and time.** Basic Rust. About 1 to 2 hours.

**Suggested labels.** `good first issue`, `enhancement`

**Done when.**

- [ ] `hivemind hooks list` shows `no-sudo` with a clear description.
- [ ] Tests cover every row of the table above, and they pass.
- [ ] `hivemind hooks enable no-sudo` works, and you turned it off again with
      `hivemind hooks disable no-sudo`.
- [ ] The four checks pass.

**Depends on.** Nothing.

---

## 5. Add a fifth built-in skill

**Why it matters.** A skill is a short set of instructions that tunes the agent
for one kind of job. Four ship today: `frontend-design`, `code-review`,
`test-writing`, and `debugging-root-cause`. This is the easiest way to help
without much Rust, because most of the work is careful writing.

**Start here.**

- `crates/harness-agent/skills/test-writing.md`: a model to copy. It has a small
  header (`id`, `name`, `description`) between `---` lines, then the
  instructions.
- `crates/harness-agent/src/skills.rs`: the `RAW` list (line ~26) that includes
  each file, and the test `the_expected_four_skills_ship` (line ~125).

**The change.** Add one new skill file, add it to `RAW`, and update the test that
lists the skills (and rename it so it does not say "four" any more). Ideas:
`refactoring` (change structure in small safe steps and keep behavior the same)
or `documentation` (write docs that match what the code really does). Please
open an issue with your idea first so two people do not write the same one.

Keep it about the same length as the others, roughly 350 to 400 words. The text
is added to the prompt whenever someone selects the skill, and prompt words cost
money. Give concrete, specific advice, not general encouragement.

**Skills and time.** Clear writing and a little Rust. About 1 to 2 hours.

**Suggested labels.** `good first issue`, `enhancement`

**Done when.**

- [ ] `hivemind skills` lists the new skill.
- [ ] The file has an `id` in lowercase with dashes that matches its filename, a
      one-line `description`, and 300 to 450 words of instructions.
- [ ] The guard test lists all five skills and no longer says "four" in its name.
- [ ] `cargo test -p harness-agent` and the other checks pass.
- [ ] *Optional:* if you have a model to try it on, say in the pull request how
      it changed the agent's behavior.

**Depends on.** Nothing.

---

## 6. Add a way to turn off the startup update check

**Why it matters.** Every time `hivemind activate` starts, it quietly asks
GitHub whether a newer release exists. That is fine for many people, but someone
running a local model for privacy, or working offline, reasonably wants it off.
Today there is no setting, and the README has to say so.

**Start here.**

- `crates/harness-cli/src/main.rs`: in `run()` (line ~716) the line
  `let update_check = banner::start_update_check();`.
- `crates/harness-cli/src/banner.rs`: `start_update_check()` and `print()`.
- `crates/harness-cli/src/update_check.rs`: `newer_version_available()`.

**The change.** Skip the check when an environment variable is set. A suggested
name is `HIVEMIND_NO_UPDATE_CHECK`, set to anything that is not empty. The
maintainers can pick a different name. Put the yes-or-no decision in a small
function that takes the value as input, so a test can check it without touching
the network. The explicit command `hivemind update` must keep working, because
the person typed it on purpose.

**Skills and time.** Rust (environment variables and a small refactor). About 1
to 2 hours.

**Suggested labels.** `enhancement`, `help wanted`

**Done when.**

- [ ] With the variable set, no request is made and no update notice is shown.
- [ ] Without it, behavior is exactly as before.
- [ ] A unit test covers the decision function: unset, empty, and set.
- [ ] The README sentence "there is no setting to turn it off yet" is replaced
      with how to turn it off.
- [ ] The four checks pass.

**Depends on.** Nothing. A matching setting in the config file would be a good
follow-up, but it is not part of this task.

---

## 7. Windows: fail clearly on a trailing `&` in `run_shell`

**Why it matters.** In a Unix shell, `npm run dev &` means "start this in the
background." On Windows, HiveMind runs commands through `cmd.exe`, where a single
`&` only means "then run the next command." So the call waits for the command to
finish. A dev server never finishes, so the agent hangs until the two-minute
time limit and then reports a failure that was not really a failure. The tool's
description already tells the model to use `background: true` instead, but when
it slips, the result is a silent two-minute stall.

**Start here.** `crates/harness-tools/src/bash.rs`:

- `Bash::execute` (line ~315): where a normal command is run.
- `shell_command` (line ~447): where Windows and Unix are told apart.
- `spawn_background` (line ~170): the correct `background: true` path. Leave
  it alone.

**The change.** On Windows only, when a command ends in a single `&`, return an
error right away that explains what happened and points to `background: true`.
Put the "does this end in a lone `&`" check in a small function that works on
every operating system, so it can be unit tested anywhere.

| Command | Result |
|---|---|
| `npm run dev &` | error |
| `npm run dev & ` (trailing space) | error |
| `a && b` | runs normally |
| `echo "a &"` | runs normally |
| any command on Unix | unchanged |

**Skills and time.** Rust, and a Windows computer to try it on. If you do not
have one, say so in the issue and someone can help test. About 2 to 3 hours.

**Suggested labels.** `bug`, `help wanted`

**Done when.**

- [ ] The helper function has unit tests, and they run and pass on every OS.
- [ ] On Windows, a command like `ping -n 30 127.0.0.1 &` now returns the
      explanation in about a second. Say in the pull request how you checked.
- [ ] `background: true` still works.
- [ ] The four checks pass.

**Depends on.** Nothing required.

---

## 8. Write "How one turn works"

**Why it matters.** The README lists the parts. Nothing walks through what
happens between pressing Enter and seeing an answer, and that map is what a new
contributor needs most. Because you are new to the code, you are the right
person to write it: you know which parts are confusing.

**Start here.** Follow one request through the code, in this order:

1. `crates/harness-cli/src/main.rs`: the `run` function that starts a session.
2. `crates/harness-agent/src/agent.rs`: `Agent::run` (line ~642), then
   `drain_stream` (line ~928), then `dispatch_and_record` (line ~989).
3. `crates/harness-tools/src/tool.rs`: `Registry::dispatch_many` (line ~266).
4. `crates/harness-provider/src/client.rs`: `ChatClient::stream`.
5. `crates/harness-agent/src/ui.rs` (the `Ui` trait) and
   `crates/harness-cli/src/ui.rs` (what draws it in the terminal).

**The change.** Add a new file, `docs/how-a-turn-works.md`. Follow one example
request from start to finish, such as "add a test for this function": the
message goes out, the model asks to read a file, the tool runs, the model edits
the file, the tests run, and the answer comes back. At each step, name the file
and the function where it happens. Where money is saved (the stable prompt
start, folding old turns into a summary, big results going to disk), say so at
the step where it happens. Then add a link to it from the "How it is built"
section of the README.

**Skills and time.** Reading Rust and clear writing. About 2 to 4 hours.

**Suggested labels.** `documentation`, `help wanted`

**Done when.**

- [ ] `docs/how-a-turn-works.md` exists and the README links to it.
- [ ] Every file and function it names really exists. A reviewer will search
      for them.
- [ ] It uses short sentences and explains any term the first time it appears.
- [ ] It reads correctly on GitHub.

**Depends on.** Nothing.
