# Contributing to HiveMind

Thanks for looking. HiveMind is about 33,000 lines of Rust across seven small
crates, with around 630 tests. That is deliberately small enough to read in a
sitting, so you should not need permission or a design doc to start.

## Getting set up

```sh
git clone https://github.com/BibhabenduMukherjee/HiveMind.git
cd HiveMind
cargo build -p harness-cli
./target/debug/hivemind --version
```

Rust stable, pinned by `rust-toolchain.toml` — no other system dependencies.

To run it you need either a hosted account (`hivemind auth login`) or your own
provider key:

```sh
export HIVEMIND_API_KEY=sk-...          # any OpenAI-compatible provider
hivemind activate --base-url https://your-provider/v1
```

You do **not** need an account to build, test, or work on most of the code.

## Before you open a PR

These four are exactly what CI runs, so running them locally means no
surprises:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

Clippy warnings are errors here. That is not fussiness — it is what keeps a
codebase this small readable by people who did not write it.

While you are iterating, `cargo check -p <crate>` on the one crate you touched
is much faster than a workspace build.

## How the code is laid out

```
crates/
  harness-types      provider-neutral wire model: Message, ToolCall, Usage, StreamEvent
  harness-config     config.toml + env resolution: model catalog, keys, policy
  harness-provider   the streaming client: SSE decode, retries, connection reuse
  harness-tools      the Tool trait, registry, parallel dispatch, fs + shell builtins
  harness-review     evidence-backed local code review
  harness-agent      the sample<->tools loop: compaction, escalation, doom-loop guard
  harness-cli        the `hivemind` binary: args, REPL, terminal UI, cost display
```

**Data flows one way, and this is the one rule we care about most.**
`harness-cli` builds a tool `Registry` and a resolved config, hands both to
`harness_agent::Agent`, which drives `harness-provider` and streams events back
through a `Ui` trait the CLI implements. **No crate reaches back up the stack.**

That constraint is why the codebase stays navigable, and it is why you can fix
something in one crate without understanding the other six. A PR that adds an
upward dependency will be asked to turn it into a trait the lower crate owns.

## What we are looking for

The Roadmap in [README.md](README.md) is the honest list of what is scoped out
and why — each item sits behind a seam that already exists. Good first
contributions tend to be:

- A new tool implementing the `Tool` trait in `harness-tools`
- A provider dialect, if you need one that is not OpenAI-compatible (add a
  module mirroring `harness-provider/src/wire.rs`, then introduce the trait
  `Agent` needs at that point — not before)
- Anything in the Roadmap
- Fixing something that annoyed you while using it

If you are planning something large, open an issue first so you do not spend a
weekend on an approach we would push back on.

## Tests

Roughly 630 of them, and they are the reason you can change code you do not
fully understand yet. Please add one for a bug you fix — a test that fails
before your change and passes after is the most useful thing in a PR.

Test names here read as sentences describing the behaviour, not the function
under test:

```rust
#[test]
fn a_ranged_read_prefers_the_note_slice_file_appended() { … }
```

That is a real convention, not a style whim: when one fails in CI, the name
alone should tell you what broke.

A few tests are `#[cfg(unix)]` because they assert POSIX shell semantics —
`cmd.exe` reads `&` as a sequential separator rather than backgrounding, so
those cases genuinely do not apply on Windows. CI runs Linux only, so if you
develop on Windows, do run the suite locally; you may find something CI cannot
see.

## Cost is a correctness property

Unusually for a coding agent, **making a change more expensive counts as a
regression.** Prompt prefixes must stay byte-stable turn to turn or prompt
caching silently stops working, and tool descriptions are re-sent on every
turn of every session, so words added there are billed forever. If your change
touches the prompt, the tool schemas, or the number of turns a task takes,
please say so in the PR.

## Reporting bugs

Open an issue with what you ran, what happened, and `hivemind --version`. For
anything security-related, read [SECURITY.md](SECURITY.md) first — please do
not open a public issue for a vulnerability.

## Licensing of contributions

Contributions are dual-licensed under Apache-2.0 or MIT, at the user's option,
matching the project. By opening a pull request you agree your contribution
may be distributed under both, unless you say otherwise.
