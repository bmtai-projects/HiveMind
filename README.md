# HiveMind

HiveMind is a coding agent that lives in your terminal. You describe a task in
plain English. It reads your code, edits files, runs commands, and tells you
what it did and what it cost.

It is written in Rust, ships as one small program called `hivemind`, and is
built around one goal: **make an agentic coding loop cheap enough to give
away.**

```text
$ hivemind activate
> add a --verbose flag to the CLI and update the tests
```

## What it does

- **Reads and edits your code.** It can search by exact text or by meaning,
  read files, and change just the lines that need changing.
- **Runs commands, with your permission.** It asks before every shell command
  unless you turn that off.
- **Shows the cost as it goes.** Every reply prints what that turn cost and
  what the session has cost so far.
- **Works with your choice of model.** Use HiveMind's own service, bring your
  own API key, or point it at a model running on your own machine.
- **Reviews code.** `hivemind review` looks at your Git changes and reports
  problems, with evidence.

## Install

macOS and Linux:

```sh
curl -fsSL https://hivemind.bmtai.in/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://hivemind.bmtai.in/install.ps1 | iex
```

The Windows installer adds `hivemind` to your PATH, so open a **new**
PowerShell window afterwards. The macOS/Linux installer puts it in
`~/.local/bin` and tells you what to add to your PATH if that folder is not
already on it.

Check that it worked:

```sh
hivemind --version
```

Later, `hivemind update` replaces your copy with the newest release.

Prefer to build it yourself? See [DEVELOPMENT.md](DEVELOPMENT.md).

## First run

HiveMind needs a model to talk to. Pick **one** of these three ways.

### 1. HiveMind's own service (easiest)

No API key of your own. You sign in and pay from a prepaid balance. See the
[pricing page](https://hivemind.bmtai.in/pricing).

```sh
hivemind auth login     # opens your browser to sign in
hivemind activate
```

`hivemind auth status` shows whether you are signed in and what your balance
is. Web search and Pro mode only work this way.

### 2. Your own API key

Any provider that speaks the OpenAI-style chat API works, for example
OpenRouter. Always pass `--base-url` and `--model` with your key.

```sh
export HIVEMIND_API_KEY=<your key>          # Windows PowerShell: $env:HIVEMIND_API_KEY = "<your key>"
hivemind activate --base-url https://openrouter.ai/api/v1 --model <a model id your provider uses>
```

> **Do not skip `--base-url`.** If you give a key but no address, HiveMind
> sends the key to its built-in default, `https://api.deepseek.com`, which is
> only right if the key is for that service.

### 3. A model on your own machine (no key, no cost)

This works with any server that speaks the OpenAI-style chat API, such as
Ollama, which serves one at `http://localhost:11434/v1` by default.

```sh
hivemind auth logout      # only if you were signed in to the hosted service
hivemind activate --base-url http://127.0.0.1:11434/v1 --model <a model you have pulled>
```

Things to know:

- HiveMind has no default local model. Use `ollama list` to see yours.
- Pick a model that supports tool calling. Without it the agent cannot read or
  edit files.
- No prices are known for your own model, so no cost is shown.
- Your code and prompts stay on your machine, but HiveMind is not fully
  offline. Every time `hivemind activate` starts, it asks GitHub whether a
  newer release exists. That is one small web request that gives up after
  under a second, and there is no setting to turn it off yet.
- If you are still signed in, `--base-url` on its own keeps using your hosted
  account and **sends it your sign-in token**. Sign out first.

### Then try it

```sh
hivemind activate                                            # interactive session
hivemind activate -p "summarize what this project does"      # one question, then exit
hivemind activate --continue                                 # pick up your last session here
hivemind activate --ui plain                                 # the old line-at-a-time REPL instead
```

Inside a session, type `/help` to see every command, and `@path/to/file` to
hand a file to the model directly.

## What works today

**Everyday use**

- Interactive sessions and one-shot prompts (`-p`).
- Saved sessions you can come back to: `--continue`, `--resume <id>`, and
  `hivemind sessions`.
- `/undo` restores files the agent edited or wrote. It does **not** undo
  shell commands, and it only remembers the current session.
- `/diff`, `/status`, `/context`, `/model`, `/reasoning`, `/skill`, `/cost`,
  `/budget`, `/compact`.
- `@path` mentions that put a file's contents straight into your message.
- `hivemind review`: reads your Git changes (`--staged`, `--base`, `--commit`,
  `--range`) and reports problems. It never edits your repository. It needs a
  model, so set one up as above first.

**Tools the agent can use**

| Tool | What it does |
|---|---|
| `read_file`, `list_dir`, `project_map` | Look at files and the shape of the project |
| `search` | Find exact text |
| `semantic_search` | Find code by meaning. Runs locally. |
| `read_program` | Several read-only lookups in one step (turned off if you have hooks configured) |
| `edit_file` | Change one exact piece of a file |
| `write_file` | Create a new file |
| `run_shell` | Run a command, after you approve it |
| `todo_write` | Keep a visible checklist for bigger jobs |
| `create_pdf`, `create_spreadsheet`, `create_diagram` | Make documents |
| `read_artifact` | Read back a large result that was saved to disk |
| `web_search`, `web_fetch` | Hosted service only, and off until you turn on `--web` |

**Safety and control**

- Files are confined to your working folder (`--workdir`, default: the current
  folder).
- Shell commands ask `[y/N]` first. `--yolo`, or the headless `-p` mode, skips
  the question, so use them carefully.
- Safety rules you can switch on with one command, such as "never force-push"
  or "only write inside `src/`": `hivemind hooks list`, then
  `hivemind hooks enable <name>`. You can also write your own in the config
  file.
- A spending cap: `--budget 0.50`, or `/budget` inside a session. It stops at
  the end of a turn, never in the middle of an edit.
- Four built-in skills that tune the agent for one kind of job: `hivemind skills`.

**A full-screen terminal interface**

`hivemind activate` opens a Ratatui-based full-screen interface by default.
`--ui plain` switches back to the plain line-at-a-time REPL, for terminals
that don't get along with raw mode (some SSH setups, unusual multiplexers).
Neither can be combined with `--protocol json` or headless `--prompt`.

The screen shows the live conversation, the model's `todo_write` checklist,
individual tool calls, changed files, real usage, and saved sessions and
reviews. Shell commands use the same approval policy as the plain REPL: a
panel shows the exact command, and `[Y]`/`[N]` approves or denies it.

Useful keys: `Enter` sends, `Shift+Enter` adds a line, `Ctrl+C` interrupts,
`Ctrl+L` clears the local display, `Ctrl+S` toggles the side panel, `Tab`
opens a tool's details, `F1`/`F2`/`F3` switch Work/Sessions/Reviews, and
`Ctrl+P` opens the command palette. The same slash commands as the plain
REPL work here too.

**For editors and other tools**

- `hivemind activate --protocol json` talks in JSON lines over stdin and
  stdout, so an editor or script can drive HiveMind.

## What is planned

These do **not** exist yet. Some are good places to help; see
[docs/community-tasks.md](docs/community-tasks.md) for small starting points.

- **An easier way to use a local model**, with a `--local` flag, and a
  `hivemind models` that asks your server what it has. Today `hivemind models`
  only prints the built-in list.
- **MCP support**, to plug in outside tool servers.
- **A `/tier` command** to switch between standard and Pro mode inside a
  session. Today you choose with `--mode` when you start.
- **A real neural embedding model for local `semantic_search`.** Today the
  local one matches on words, not meaning. Pro mode on the hosted service
  already uses a stronger, hosted one.
- **Support for providers that do not use the OpenAI-style chat API.**

## Why it is cheap

Most of what a coding agent costs is not the model. It is **re-sending the
whole conversation** on every turn. A few things fix most of that:

| What | How it helps | Where to read the code |
|---|---|---|
| **Stable prompt prefix** | The start of every request stays identical, so providers that cache can charge the cheap "cached" rate for it. Cached input costs a small fraction of normal input on providers that support it. HiveMind shows the cache hit rate live. | [`wire.rs`](crates/harness-provider/src/wire.rs), [`tool.rs`](crates/harness-tools/src/tool.rs) |
| **Compaction** | When a session fills 75% of the model's memory (you can change that), older turns are folded into one summary instead of being re-sent forever. | [`compaction.rs`](crates/harness-agent/src/compaction.rs) |
| **Small edits** | `edit_file` swaps one exact piece of text instead of rewriting the whole file. Output tokens are the most expensive kind, so this saves the most. | [`edit.rs`](crates/harness-tools/src/edit.rs) |
| **Big results go to disk** | A huge command output is saved as a file, and the conversation keeps a short preview and a handle. `read_artifact` fetches more if needed. | [`artifact.rs`](crates/harness-tools/src/artifact.rs) |
| **Cheap first, stronger only when stuck** | On the hosted service, a task starts on the cheap default model. If the agent repeats itself or keeps hitting errors, it switches to a stronger model for that task only. | [`agent.rs`](crates/harness-agent/src/agent.rs) |
| **Parallel tools** | When the model asks for several tools at once, they run at the same time. Results are put back in order. | [`tool.rs`](crates/harness-tools/src/tool.rs) |
| **Retries with backoff** | Rate limits and network hiccups are retried with growing, randomized waits, and a server's `Retry-After` is respected. | [`retry.rs`](crates/harness-provider/src/retry.rs) |
| **Reused connections, no copies** | One connection pool for the whole run, and the conversation is not copied to send a turn. | [`client.rs`](crates/harness-provider/src/client.rs) |
| **Live cost readout** | Every reply prints what that turn and the session cost. | [`ui.rs`](crates/harness-cli/src/ui.rs) |

## How it is built

Follow one request from Enter to the answer in [How a turn works](docs/how-a-turn-works.md).

HiveMind is a Rust workspace of seven small crates:

```text
crates/
  harness-types      the shared shapes: Message, ToolCall, Usage, StreamEvent
  harness-config     config file and environment: model catalog, keys, settings
  harness-provider   talks to the model: streaming, retries, connection reuse
  harness-tools      the Tool trait and every built-in tool
  harness-review     Git diff and review report types (no other crate needed)
  harness-agent      the loop: model -> tools -> model, plus compaction and undo
  harness-cli        the `hivemind` program: arguments, terminal, cost display
```

Who depends on whom (an arrow means "uses"):

```text
harness-cli ──► harness-agent ──► harness-provider ──► harness-types
     │               │      └────► harness-tools ─────► harness-types
     │               └───────────► harness-review
     └──► harness-config  (the agent uses it too)
```

Only the main arrows are drawn. The CLI also uses `harness-tools`,
`harness-provider`, `harness-review`, and `harness-types` directly. You can
see the exact list in each crate's `Cargo.toml`.

**The one rule:** arrows only point down. `harness-cli` builds the tools and
the settings and hands them to `harness_agent::Agent`. The agent talks to the
model and reports progress back through a `Ui` trait that the CLI implements.
(A trait is Rust's word for an interface: a list of things a type promises to
do.) No lower crate reaches back up. That is what lets you change one crate
without understanding the others.

One client, [`ChatClient`](crates/harness-provider/src/client.rs), speaks the
OpenAI-style chat API to every backend. There is no `Provider` trait yet, on
purpose: with only one client it would just add a layer for nothing. If a
provider ever needs a different request and response format, it would get its
own module, like [`wire.rs`](crates/harness-provider/src/wire.rs), and that is
the moment to add the trait.

The hosted service (accounts, billing, and the model proxy) is a separate
private service. Nothing in this repository needs it, and the second and
third ways to connect never talk to it. Ready-made downloads are attached to
this repository's [releases](https://github.com/bmtai-projects/HiveMind/releases).

## Settings

You do not need a config file. To change defaults, copy
[`config.example.toml`](config.example.toml) to
`~/.config/hivemind/config.toml` (on Windows that is
`%USERPROFILE%\.config\hivemind\config.toml`). Every setting there is optional
and explained in comments.

`hivemind activate --help` lists every flag. The most useful:

| Flag | What it does |
|---|---|
| `-p, --prompt` | Run one prompt and exit |
| `--model` | Start on a specific model |
| `--budget` | Stop once estimated spend reaches this many USD |
| `--continue` / `--resume <id>` | Return to a saved session |
| `--skill` | Start with a skill selected |
| `--yolo` | Do not ask before running shell commands |
| `--ui plain` | Use the old line-at-a-time REPL instead of the full-screen interface |

## Contributing

Contributions of every size are welcome, and you do not need to be a Rust
expert. Bug reports, clearer docs, new tests, and small fixes all help.

- **New here?** Read [CONTRIBUTING.md](CONTRIBUTING.md). It covers choosing a
  task, making a change, checking it, and opening a pull request.
- **Want a small first task?** Start with
  [docs/community-tasks.md](docs/community-tasks.md), or look for the
  `good first issue` label.
- **Setting up your machine?** [DEVELOPMENT.md](DEVELOPMENT.md) has the build
  and test commands. Every check runs without a paid API key.
- **Found a bug or have a question?** Open an issue. There is a form for each.
- **Found a security problem?** Please do not open a public issue. Follow
  [SECURITY.md](SECURITY.md).

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Contributions are dual-licensed the same way unless you say
otherwise.
