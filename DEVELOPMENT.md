# Development

How to build, test, and run HiveMind on your own machine.

- **Part 1** is for everyone who wants to change the code.
- **Part 2** is only for maintainers who publish releases. You can ignore it.

---

# Part 1: Working on the code

## What you need

- **Git.**
- **Rust, the latest stable version.** Install it with [rustup](https://rustup.rs).
  The project uses the 2024 edition, so a Rust from an old system package may
  not compile it. The file `rust-toolchain.toml` asks for `stable`, and rustup
  reads it for you.
- **A C toolchain**, which Rust needs to link programs:
  - Windows: Visual Studio Build Tools with the "Desktop development with C++"
    option.
  - macOS: run `xcode-select --install`.
  - Linux: `build-essential` on Debian and Ubuntu, or your distro's version.

You do **not** need an API key, an account, or OpenSSL to build and test.
HiveMind uses `rustls`, not OpenSSL.

## Get the code and build it

```sh
git clone https://github.com/bmtai-projects/HiveMind.git
cd HiveMind
cargo build -p harness-cli
```

If you are working from your own fork, clone that instead. The first build
downloads and compiles every dependency, so it takes a few minutes. Later
builds are fast.

Run what you built:

```sh
./target/debug/hivemind --version         # macOS and Linux
.\target\debug\hivemind.exe --version     # Windows PowerShell
cargo run -p harness-cli -- --version     # any system; everything after -- goes to hivemind
```

A few more build commands:

```sh
cargo check -p harness-agent              # type-check one crate only, the fastest feedback
cargo build --release -p harness-cli      # optimized build; slower, and rarely needed
```

## The checks to run before a pull request

These are the same commands CI runs. If they pass on your machine, CI should
pass too.

```sh
cargo fmt --all                                        # fixes formatting in place
cargo clippy --workspace --all-targets -- -D warnings  # lint; warnings count as errors
cargo test --workspace                                 # every test
cargo build --workspace --release                      # CI also does a release build
```

Three tips:

- To see formatting problems **without** changing files, run
  `cargo fmt --all -- --check`. That is what CI runs.
- The checks stop at the first failure. A formatting mistake hides everything
  after it, so a red CI run that finishes in seconds is almost always
  formatting. Fix that first.
- To run a single test, give part of its name:
  `cargo test -p harness-config a_base_url`.

Which operating systems CI covers is written in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml). If you use a system CI
does not cover, running the tests yourself is the only way to catch problems
that appear there.

## Which checks need a paid API?

None of the checks above do.

| What you run | Needs an API key or account? |
|---|---|
| `cargo fmt`, `cargo clippy`, `cargo build` | No |
| `cargo test --workspace` | **No.** The tests use fake servers on your own machine, and none reads a real key. The whole suite passes from a fresh clone with no keys and an empty home folder. |
| `cargo test --workspace -- --ignored` | Not paid, but they need extras. The diagram tests need `mmdc` (Mermaid CLI) installed. Two others audit the real repository checkout and are meant to be run by hand. Skip these unless you are changing those tools. |
| Running `hivemind activate` yourself | Needs **some** model: the hosted service (paid balance), your own key, or a free model on your own machine. See below. |
| `hivemind review` on real changes | Needs a model, same as above. With nothing to review it needs nothing. |

## Running HiveMind while you develop, for free

You can point HiveMind at a model on your own machine. With
[Ollama](https://ollama.com), for example:

```sh
ollama pull <a model that supports tool calling>
ollama list                                # shows the exact names you can use
./target/debug/hivemind activate --base-url http://127.0.0.1:11434/v1 --model <name from ollama list>
```

Two things to watch: sign out of the hosted service first (`hivemind auth
logout`), because while you are signed in `--base-url` keeps using your
account. And use a model that supports tool calling, or the agent cannot read
or edit files. The [README](README.md#3-a-model-on-your-own-machine-no-key-no-cost)
explains the details.

To try your build, run it from `target/`. Do not copy it over your installed
`hivemind`. `hivemind update` would replace it with the latest release anyway.

The terminal screen cannot be checked by a script, so look at it yourself when
you change it. Start a session and try `/help`, `/cost`, `/compact`, typing `@`
and pressing Tab to complete a file name, and Ctrl-D to leave.

## Where to look

| To change... | Start in |
|---|---|
| The command line, the startup flow | `crates/harness-cli/src/main.rs` |
| Slash commands like `/help` and `/undo` | `crates/harness-cli/src/commands.rs` |
| How the terminal looks | `crates/harness-cli/src/ui.rs` |
| The agent loop | `crates/harness-agent/src/agent.rs` (`Agent::run`) |
| A tool the agent can use | `crates/harness-tools/src/`, plus where tools are registered in `crates/harness-cli/src/main.rs` |
| Talking to the model | `crates/harness-provider/src/` |
| Config keys and defaults | `crates/harness-config/src/lib.rs`, and `config.example.toml` |
| Skills | `crates/harness-agent/skills/*.md`, listed in `crates/harness-agent/src/skills.rs` |
| Safety presets (`hivemind hooks`) | `crates/harness-cli/src/hook_presets.rs` |
| Code review | `crates/harness-review/`, and `crates/harness-agent/src/review_orchestrator.rs` |

The [README](README.md#how-it-is-built) explains how the crates fit together.

## Common problems

**A clippy error you do not understand.** Read the `help:` line under it.
Rust usually shows the fix. For example, the lint
`manual_pattern_char_comparison` wants `s.trim_end_matches(['a', 'b'])` instead
of a closure.

**`no method named ... found` for something that exists.** The trait that
provides the method is probably not imported. Add the `use` line the compiler
suggests.

**Windows: `linking with link.exe failed` and `link: extra operand`.** Git for
Windows ships a Unix tool that is also called `link`, and it can be picked up
instead of Microsoft's linker when Visual Studio's C++ tools cannot be found.
Install "Desktop development with C++" from the Visual Studio Build Tools, and
build from PowerShell or a Developer Command Prompt.

**Windows: a test fails but CI is green.** CI may not cover your system. A test
that uses `sleep`, a trailing `&`, or other Unix shell features only makes
sense on Unix, so it should be marked `#[cfg(unix)]`. `cmd.exe` treats `&` as
"run the next command after this one", not "run in the background".

**`Cargo.lock` conflicts after a rebase.** Do not edit it by hand. Take either
version, then run `cargo check --workspace` and it will fix itself.

**`warning: LF will be replaced by CRLF`.** This is Git talking about line
endings on Windows. It is harmless.

**`error: workdir "...": The system cannot find the path`.** The folder you
gave to `--workdir` must already exist.

---

# Part 2: Releasing (maintainers only)

Regular contributors do not need any of this. Releases are published by
maintainers with write access.

## How it fits together

- Source and compiled downloads both live in this repository. Releases are
  attached here, so there is no second repository to keep in step.
- The install scripts, [`install.sh`](install.sh) and
  [`install.ps1`](install.ps1), are in this repository too, which means they
  are reviewed in pull requests like any other file. The website serves
  `https://hivemind.bmtai.in/install.sh` and `install.ps1` by passing straight
  through to the copies here.
- Pushing a tag that starts with `v` runs
  [`release.yml`](.github/workflows/release.yml). It has three stages, each
  waiting for the one before: `check` (the same checks as CI), then `build`
  (five platforms), then `release` (publishes). If `check` fails, nothing is
  published.
- Publishing to this repository needs nothing but the built-in
  `GITHUB_TOKEN`, which the workflow grants `contents: write`.
- There is one exception. Binaries released **before** the move to the `bmtai`
  organisation have the old `HiveMind-releases` address compiled into them and
  ask that repository for updates. So `release.yml` also mirrors each release
  there, and only that step needs the `RELEASES_REPO_TOKEN` secret, because
  writing to another repository is outside `GITHUB_TOKEN`'s reach. Without the
  mirror, those older installs would answer "you are up to date" forever. The
  step can be deleted once anyone still on such a build is expected to
  reinstall instead.

## Before you start

- You can push to `main` and push tags.
- The [GitHub CLI](https://cli.github.com) is installed and signed in:
  `gh auth status`.
- **Version numbers only go up.** `hivemind update` offers a release only if
  its number is higher than the one running, and that check is already inside
  every installed copy. A lower number would leave people stuck.

## Steps

```sh
# 1. Be sure main is green. Then bump the version: edit `version` under
#    [workspace.package] in the root Cargo.toml.

# 2. Let Cargo update the lock file to match.
cargo check --workspace

# 3. Commit those two files, and nothing else.
git add Cargo.toml Cargo.lock
git commit -m "chore(release): vX.Y.Z"
git push origin main

# 4. Wait for CI to go green on that commit.
gh run watch --repo bmtai-projects/HiveMind

# 5. Tag it. Pushing the tag starts the release.
git tag vX.Y.Z
git push origin vX.Y.Z

# 6. Watch it. Expect about 10 to 15 minutes.
gh run list --repo bmtai-projects/HiveMind --workflow Release --limit 1
gh run watch <run-id> --repo bmtai-projects/HiveMind
```

Add only those two files in step 3. Using `git add -A` or `git commit -a`
can sweep unrelated edits into a release commit, and that has broken a release
before.

## Check that it worked

```sh
# The real result. "success" is the only good answer.
gh run view <run-id> --repo bmtai-projects/HiveMind --json status,conclusion --jq '{status, conclusion}'

# Five archives plus SHA256SUMS.txt should be attached.
gh release view vX.Y.Z --repo bmtai-projects/HiveMind

# The public installer works from scratch. Use a throwaway folder.
HIVEMIND_INSTALL_DIR="$(mktemp -d)" bash -c "$(curl -fsSL https://hivemind.bmtai.in/install.sh)"
```

On Windows, set `$env:HIVEMIND_INSTALL_DIR` to a throwaway folder and run
`irm https://hivemind.bmtai.in/install.ps1 | iex`.

## If a release fails

If the run failed before anything was published, you can remove the tag and
try again:

```sh
git tag -d vX.Y.Z                       # local
git push origin --delete vX.Y.Z         # remote
# fix the problem, commit, push to main, wait for green, then tag again
git tag vX.Y.Z
git push origin vX.Y.Z
git rev-parse vX.Y.Z HEAD               # must print the SAME hash twice
```

If a release **was** published, never reuse its number. People may already have
it. Publish the next patch version instead.

## Maintainer problems, and their fixes

**`403 Resource not accessible by personal access token` when publishing.**
A fine-grained token failed here, both when generating release notes and when
creating the release. Use a **classic** token with the `repo` scope, and set it
without putting it in your shell history:

```sh
gh secret set RELEASES_REPO_TOKEN --repo bmtai-projects/HiveMind
```

Paste the token when asked. Do not pass it as an argument.

**The tag points at an old commit after you rewrote history.** Pushing a tag
that already exists does not warn you. Before trusting a tag push, check that
`git rev-parse vX.Y.Z` and `git rev-parse HEAD` match. If not, delete and
recreate the tag as shown above.

**The installer looks wrong from `raw.githubusercontent.com`.** GitHub caches
raw files for a few minutes after a push. Wait, or read it through the API,
which skips the cache:
`gh api repos/bmtai-projects/HiveMind/contents/install.sh --jq .content | base64 -d`.

**`gh run watch` finished but you are unsure.** The command's own exit code
only says the *watch* worked. Read the `conclusion` with the command in "Check
that it worked".

**`unbound variable` at the very end of `install.sh`.** A `local` variable used
by an `EXIT` trap is already gone when the trap runs, and `set -u` treats that
as fatal even though the install succeeded. Make the variable script-global.
This is already fixed. It is written down in case it comes back.

**A commit shows the wrong author.** A new machine may not have your identity
set. Check with `git log -1 --format="%an <%ae>"`. Set it for this repository
only with `git config user.name "Your Name"` and
`git config user.email "you@example.com"`. To fix a commit you have not shared
yet, run `git commit --amend --reset-author`.
