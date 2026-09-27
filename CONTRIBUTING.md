# Contributing to HiveMind

Thank you for wanting to help. HiveMind is a small project on purpose. A few
small crates, lots of tests, and code you can read in a sitting. You do not
need to be a Rust expert, and you do not need anyone's permission to start with
something small.

## Ways to help

Every one of these counts as a real contribution:

- **Report a bug.** Something broke or surprised you? Tell us.
- **Improve the docs.** Fix a confusing sentence, a wrong command, or a typo.
- **Add a test.** Tests are what let people change code safely.
- **Fix something small.** An unclear error message, a stale number, a missing
  check.
- **Try it on your system** and tell us what happened, especially on Windows,
  macOS, or with a local model.
- **Ask a question.** If something was hard to understand, that is useful to
  know.

## Choose something to work on

1. **Want a small first task?** Look in
   [docs/community-tasks.md](docs/community-tasks.md), or for issues labeled
   `good first issue`. Each one names the files to open and says how you will
   know you are done. Issues labeled `help wanted` are a bit bigger.
2. **Have your own idea?** For a small fix (a typo, a clearer message, a missing
   test), go ahead and open a pull request. For anything bigger, such as a new
   feature, a change to how the agent behaves, or a new dependency, **open an
   issue first** and describe what you want to do. It saves you effort if the
   idea does not fit.
3. **Tell us you are on it.** Comment on the issue, so two people do not do the
   same job. If your plans change, say so and someone else can take it.

## Make a change

1. **Fork** the repository on GitHub, then clone your fork.
2. **Set up your machine.** [DEVELOPMENT.md](DEVELOPMENT.md) has every step. The
   short version:

   ```sh
   cargo build -p harness-cli
   ```

3. **Make a branch** for your change:

   ```sh
   git switch -c my-change
   ```

4. **Make the change.** Keep it small and about one thing. A small pull request
   is reviewed faster and is easier to get right.
5. **Check it** (next section).
6. **Commit.** Start with a short line that says what the change does, like
   "Show a clear error when the working folder is missing". Add a few lines
   about *why* if it is not obvious.
7. **Push** your branch and open a pull request.

## Check it

Run these four before you open a pull request. They are exactly what CI runs.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

**All four work without an API key or any paid account.** The tests use fake
servers on your own machine. See [DEVELOPMENT.md](DEVELOPMENT.md) for the
details, and for what to do if a check fails.

Formatting runs first, and a failure there hides everything after it. If CI
finishes in a few seconds and is red, it is almost always formatting. Run
`cargo fmt --all` and push again.

**Please add a test when you fix a bug.** A test that fails before your change
and passes after it is the most useful thing a pull request can contain. Name
tests as sentences that say what should be true, so the name alone explains a
failure:

```rust
#[test]
fn a_base_url_without_any_key_resolves_to_a_local_backend() { ... }
```

Some tests only make sense on Unix, for example ones that use `sleep` or a
trailing `&`. Mark those `#[cfg(unix)]`. CI may not run on your system, so if
you use Windows or macOS, running the tests yourself is how you catch problems
there.

## Open a pull request

- A template appears when you open the pull request. It is short. Fill in what
  changed and why.
- If your change fixes an issue, write `Fixes #123` so it closes by itself.
- **CI must be green.** If it is red, open the failed step and read the message.
  Most of the time it is formatting or a clippy warning.
- **Unfinished is fine.** Open a draft pull request any time you want early
  feedback or are unsure about the direction.

**What happens next.** A maintainer reads your pull request and may ask for
changes. That is normal. It is not a rejection, and it happens to everyone. Push
more commits to the same branch to update it. When it looks good, a maintainer
merges it. Reviews are done by people in their own time, so please be patient.
A polite nudge after about a week is fine.

## Ask questions

There are no silly questions here. If you are stuck or something is unclear:

- **Open an issue and choose "Question".** There is a short form for it. Say
  what you are trying to do and what you have tried.
- Look through existing issues first. Someone may have asked already.
- **Trouble with your HiveMind account or billing** is a different matter. That
  goes to hivemind@bmtai.in, not to the issue tracker.
- **Security problems** go through [SECURITY.md](SECURITY.md), never in a public
  issue.

## Reporting a bug

Open an issue and choose **Bug report**. The form asks for the few things that
help most: what happened, what you expected, the steps to reproduce it, and the
output of `hivemind --version`. Before you paste anything, remove API keys and
private code.

## How the code is organized

There are seven crates. The [README](README.md#how-it-is-built) has the picture.
You only need to remember one rule:

> **Data flows one way. A crate may use crates below it, and never crates above
> it.** `harness-cli` builds the tools and settings and gives them to
> `harness-agent`, which talks to the model. Nothing lower reaches back up.

That rule is why you can fix something in one crate without understanding the
other six. A change that adds an upward dependency will be asked to turn it into
a trait that the lower crate owns.

## Things worth knowing

- **Cost is part of correctness.** HiveMind exists to be cheap, so making it
  more expensive counts as a regression. The start of every request must stay
  identical from turn to turn, or provider caching stops working. Tool
  descriptions are sent on **every turn of every session**, so extra words there
  cost real money. If your change touches the prompt, the tool descriptions, or
  how many turns a task takes, please say so in the pull request.
- **Comments should be short.** Prefer a clear name over a comment. When you do
  comment, say *why*, not *what*.
- **Formatting is not a debate.** `rustfmt` decides, and clippy warnings are
  errors.

## Be kind

Be patient with newcomers, assume good intentions, and give feedback on the
code, never on the person. If someone is being unkind, tell a maintainer by
opening an issue or writing to hivemind@bmtai.in.

## License

Contributions are dual-licensed under Apache-2.0 or MIT, at the user's option,
the same as the project. By opening a pull request you agree that your
contribution may be distributed under both, unless you say otherwise.
