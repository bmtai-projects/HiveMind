# HiveMind

HiveMind is an AI coding assistant that works in your terminal. You tell it
what you want in plain English. It reads your code, edits files, runs
commands (after asking you), and tells you what it did and what it cost.

It is one small program, `hivemind`, for Windows, macOS and Linux. Use it with
HiveMind's own service, with your own API key from a provider such as
OpenRouter, or with a free model running on your own computer.

```text
$ cd my-project
$ hivemind activate
> add a --verbose flag to the CLI and update the tests
```

**On this page:** [Install](#1-install) ·
[Connect a model](#2-connect-a-model) · [Use it](#3-use-it) ·
[Settings](#settings) · [Troubleshooting](#troubleshooting) ·
[Contributing](#contributing)

## 1. Install

**Windows** (PowerShell):

```powershell
irm https://hivemind.bmtai.in/install.ps1 | iex
```

Then close PowerShell and open a **new** window, so it can find `hivemind`.

**macOS and Linux:**

```sh
curl -fsSL https://hivemind.bmtai.in/install.sh | bash
```

This puts `hivemind` in `~/.local/bin`. If that folder is not on your PATH,
the installer prints the line to add.

Check that it worked:

```sh
hivemind --version
```

- **Update later:** `hivemind update` replaces your copy with the newest
  release.
- **Build it yourself:** see [DEVELOPMENT.md](DEVELOPMENT.md).
- **Uninstall:** delete the program, `%LOCALAPPDATA%\hivemind` on Windows
  (also remove it from your user PATH) or `~/.local/bin/hivemind` on macOS and
  Linux. Your settings and saved sessions are in `~/.config/hivemind`
  (Windows: `%USERPROFILE%\.config\hivemind`); delete that folder too if you
  want them gone.

## 2. Connect a model

HiveMind needs a language model to do the thinking. Pick **one** of these:

| | Cost | What you need | Good for |
|---|---|---|---|
| **A. HiveMind's service** | Pay as you go, from a prepaid balance | A HiveMind account | Starting fast, web search |
| **B. Your own API key** | Your provider's prices. OpenRouter also has free models. | A key from OpenRouter, OpenAI, Anthropic, DeepSeek or similar | Choosing from hundreds of models |
| **C. A model on your computer** | Free | [Ollama](https://ollama.com) (or similar) and a reasonably fast computer | Privacy, no account |

The first time you run `hivemind activate`, with nothing set up yet, it shows
a menu that does A or B for you:

```text
No backend configured yet. Pick one:

  1) HiveMind (hosted) — sign in, pay-as-you-go, no key needed
  2) OpenRouter
  3) OpenAI
  4) Anthropic (Claude)
  5) DeepSeek
  6) Other (any OpenAI-compatible endpoint)
```

### A. HiveMind's own service

```sh
hivemind auth login     # opens your browser to sign in
hivemind activate
```

- `hivemind auth status` shows whether you are signed in and your balance.
- `hivemind models` lists the models you can choose with `--model`.
- Prices are on the [pricing page](https://hivemind.bmtai.in/pricing).
- `hivemind auth logout` signs you out.

### B. Your own API key

1. Get an API key from your provider's website. For OpenRouter, sign up at
   [openrouter.ai](https://openrouter.ai) and create one under **Keys**.
2. Run `hivemind activate`, choose your provider from the menu, and paste the
   key. It is hidden as you type.
3. If asked, type a model id. OpenRouter and DeepSeek have a default, so you
   can just press on.

That's it: the choice is saved, and from now on `hivemind activate` uses it.

| Provider | Address (`--base-url`) | Model id to start with |
|---|---|---|
| OpenRouter | `https://openrouter.ai/api/v1` | `deepseek/deepseek-v4-flash` (the default) |
| OpenAI | `https://api.openai.com/v1` | one from OpenAI's model list |
| Anthropic | `https://api.anthropic.com` (also needs `--dialect anthropic`) | one from Anthropic's model list |
| DeepSeek | `https://api.deepseek.com` | `deepseek-v4-flash` (the default) |
| Other | your provider's address | any service with an OpenAI-style chat API |

Good to know:

- **Not sure which model?** Start with the default, then type `/model` in a
  session. With OpenRouter it opens on a **Top 5** and lets you sort every
  model by score, price or context size.
- **Free models:** on OpenRouter, models whose id ends in `:free` cost
  nothing but have a daily request limit.
- **Where the key goes:** the menu saves it as plain text in your config file
  (see [Settings](#settings)). To remove it, delete the `api_key` line there.
- **Switching provider later:** the menu only appears when nothing is set up.
  To change provider, edit the `[model]` section of your config file. Delete
  that section and `hivemind activate` shows the menu again.

**Without the menu:** put the key in an environment variable and give the
address and model each time.

```sh
# macOS / Linux
export HIVEMIND_API_KEY=<your key>
hivemind activate --base-url https://openrouter.ai/api/v1 --model deepseek/deepseek-v4-flash
```

```powershell
# Windows: setx keeps the key for future windows, $env: sets it for this one
setx HIVEMIND_API_KEY "<your key>"
$env:HIVEMIND_API_KEY = "<your key>"
hivemind activate --base-url https://openrouter.ai/api/v1 --model deepseek/deepseek-v4-flash
```

> **Always pass `--base-url` with a key.** Without it, HiveMind sends the key
> to its built-in default, `https://api.deepseek.com`, which is only right if
> it is a DeepSeek key.

### C. A model on your own computer

Free, and your code and prompts stay on your machine. It is slower than a
cloud model, especially on a laptop without a graphics card.

**1. Install Ollama** from [ollama.com](https://ollama.com) and start it.

**2. Download a model that can use tools**, for example:

```sh
ollama pull qwen2.5-coder:7b     # about 5 GB; this is the one we test with
```

Bigger models do better work if your computer can run them.

**3. Give the model enough room.** Ollama's default context is 4,096 tokens.
HiveMind's instructions are longer than that, so Ollama silently cuts them
and the model gets confused, for example printing JSON instead of doing
anything. Raise it to 16k, and keep the model loaded between messages:

- **Windows:** run these, then quit Ollama from the taskbar and start it
  again:

  ```powershell
  setx OLLAMA_CONTEXT_LENGTH 16384
  setx OLLAMA_KEEP_ALIVE 1h
  ```

- **macOS:** run these, then quit and reopen the Ollama app:

  ```sh
  launchctl setenv OLLAMA_CONTEXT_LENGTH 16384
  launchctl setenv OLLAMA_KEEP_ALIVE 1h
  ```

- **Linux** (Ollama running as a service): run `sudo systemctl edit ollama.service`,
  add the lines below, then `sudo systemctl restart ollama`:

  ```ini
  [Service]
  Environment="OLLAMA_CONTEXT_LENGTH=16384"
  Environment="OLLAMA_KEEP_ALIVE=1h"
  ```

**4. Connect HiveMind:**

```sh
hivemind activate --base-url http://127.0.0.1:11434/v1 --model qwen2.5-coder:7b
```

This works even if you saved an API key for another provider: HiveMind
never sends a key to a server on your own machine unless you give one for it
with `--api-key`.

To make the local model your default, set the `[model]` section of your
config file (see [Settings](#settings)) to:

```toml
[model]
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5-coder:7b"
```

If nothing is set up yet, the menu's **Other** option writes this for you:
enter the address, leave the key empty, and type the model name.

> **Signed in to HiveMind's service?** Sign out first
> (`hivemind auth logout`). While you are signed in, HiveMind uses your
> sign-in with that address instead of treating it as a local model.

What to expect:

- **The first message is the slow one.** The model must first read about
  3,000 tokens of instructions and tool descriptions. On a laptop CPU that
  can take a few minutes. After that, replies take seconds while the model
  stays loaded.
- **A smaller set of tools**, to keep that first read short: no PDF,
  spreadsheet, diagram or semantic-search tools. To give local models every
  tool (say, a big model on a GPU), set `local_all_tools = true` under
  `[agent]` in your config file.
- **If HiveMind says the model "read only N tokens"**, the context is still
  too small. Check step 3, and that you restarted Ollama.
- Some models write their tool calls as plain text. HiveMind spots those and
  runs them.
- No cost is shown, since nothing is billed.
- Other local servers work the same way. LM Studio's address is usually
  `http://127.0.0.1:1234/v1`.
- HiveMind is not fully offline: each time it starts, it asks GitHub whether
  a newer release exists. That one small request gives up after under a
  second.

## 3. Use it

Open a terminal **in your project folder** and start a session:

```sh
cd my-project
hivemind activate
```

HiveMind only reads and writes files inside that folder (or the one you give
with `--workdir`). Type what you want, for example:

- `explain what this project does`
- `fix the failing test in tests/parser.rs`
- `add input validation to the signup form`
- `@src/main.rs why does this crash on empty input?` (`@` hands a file to
  the model; press Tab to complete the name)

Before it runs any shell command, HiveMind shows you the exact command and
waits for `Y` or `N`.

**Other ways to start**

| Command | What it does |
|---|---|
| `hivemind activate -p "summarize this project"` | Answer one request and exit. Shell commands run **without asking** in this mode. |
| `hivemind activate --continue` | Pick up your last session in this folder |
| `hivemind sessions`, then `hivemind activate --resume <id>` | Go back to an older session |
| `hivemind review --staged` | Review your Git changes (also `--base`, `--commit`, `--range`). It never edits anything. |
| `hivemind activate --ui plain` | A simple line-by-line mode, if the full-screen one misbehaves in your terminal |

**Keys in a session**

| Key | What it does |
|---|---|
| `Enter` / `Shift+Enter` | Send / new line |
| `Esc` | Stop the current run |
| `Ctrl+C` | Interrupt, or clear the message box |
| `Tab`, then `↑` `↓` `PgUp` `PgDn` or the mouse wheel | Move between panels and scroll them |
| `Enter` on a tool call or a changed file | Open its full result, or its diff |
| `Ctrl+O` | Show everything, with tool calls expanded |
| `Shift`+drag | Select text (`Option`+drag in iTerm2) |
| `?` | List every key |

**Commands in a session** (type `/` to see them all)

| Command | What it does |
|---|---|
| `/model` | Choose a model. With your own key, the list comes from your provider. |
| `/reasoning` | Choose how much the model thinks before it answers |
| `/undo [n]` | Undo the last `n` turns. Restores files it edited or wrote; it cannot undo shell commands. |
| `/diff` | Every change this session made |
| `/cost` | What this session has cost |
| `/budget 0.50` | Stop once this session reaches $0.50 (`/budget off` to remove) |
| `/compact` | Fold older messages into a summary, to save tokens |
| `/skill` | Tune it for one kind of job (`hivemind skills` lists them) |
| `/web on` | Let it search the web (HiveMind's service only) |
| `/status`, `/context` | Session details, and how full the model's memory is |
| `/sessions`, `/reviews` | Saved sessions and saved reviews |
| `/copy`, `/transcript` | Copy the last reply; show the whole conversation |
| `/help`, `/exit` | All commands; quit |

**What the agent can do**

| Tool | What it does |
|---|---|
| `read_file`, `list_dir`, `project_map` | Look at files and the shape of the project |
| `search`, `semantic_search` | Find exact text, or code by meaning |
| `edit_file`, `write_file` | Change part of a file, or create one |
| `run_shell` | Run a command, after you approve it |
| `todo_write` | Keep a visible checklist for bigger jobs |
| `create_pdf`, `create_spreadsheet`, `create_diagram` | Make documents |
| `web_search`, `web_fetch` | HiveMind's service only, and only after `/web on` or `--web` |

Reading PDFs needs a build with `--features pdf`. The official downloads
don't include it yet.

**Staying in control**

- Files outside your working folder are off limits.
- Shell commands wait for your `Y`. `--yolo` (and `-p` mode) skip that, so
  use them with care.
- Spending caps: `--budget 0.50` when you start, or `/budget` in a session.
  It stops between steps, never in the middle of an edit.
- Ready-made safety rules, such as "never force-push" or "only write inside
  `src/`": run `hivemind hooks list`, then `hivemind hooks enable <name>`.

## Settings

You don't need a config file. HiveMind keeps its files in
`~/.config/hivemind` (Windows: `%USERPROFILE%\.config\hivemind`):

| File | What's in it |
|---|---|
| `config.toml` | Your settings, and the provider and key the menu saved |
| `credentials.toml` | Your HiveMind sign-in (`hivemind auth login`) |
| `sessions/` | Saved conversations, for `--continue` and `--resume` |

To change defaults, copy [`config.example.toml`](config.example.toml) to
`config.toml` there. Every setting is optional and explained in comments.
`--config <file>` uses a different file.

`hivemind activate --help` lists every flag. The most useful:

| Flag | What it does |
|---|---|
| `--model <id>` | Start on a specific model |
| `--base-url <url>` | Talk to a different provider or server |
| `--reasoning-effort <level>` | How much the model thinks, if it supports that |
| `--budget <usd>` | Stop once the session's estimated spend reaches this |
| `--skill <id>` | Start with a skill selected |
| `--web` | Allow web search (HiveMind's service only) |
| `--workdir <folder>` | Work in a different folder |
| `--yolo` | Don't ask before running shell commands |
| `--ui plain` | Use the simple line-by-line mode |

## Troubleshooting

**`hivemind` is not recognized, or "command not found".** On Windows, open a
new PowerShell window after installing. On macOS and Linux, add
`~/.local/bin` to your PATH as the installer said.

**The provider menu doesn't appear.** It only appears when nothing is set up.
To change provider, edit or delete the `[model]` section in `config.toml`.

**"401", "unauthorized" or "invalid API key".** The key and `--base-url` must
belong to the same provider. If you gave a key without `--base-url`, it went
to DeepSeek.

**A local model prints JSON, ignores the task, or HiveMind says it "read
only N tokens".** Its context is too small. Follow step 3 of
[section C](#c-a-model-on-your-own-computer), then restart Ollama.

**A local model is very slow.** The first message is the slow one on a CPU.
Set `OLLAMA_KEEP_ALIVE=1h` so the model isn't unloaded between messages, or
try a smaller model.

**An OpenRouter `:free` model stops answering.** Free models have a daily
request limit. Wait until the next day, or use a paid model.

**The screen looks wrong, or keys don't work.** Some terminals and SSH setups
don't get along with the full-screen mode. Use `hivemind activate --ui plain`.

**I can't select text.** Hold `Shift` while dragging (`Option` in iTerm2).

Still stuck? [Open an issue](https://github.com/bmtai-projects/HiveMind/issues).

## How it is built

This part is for contributors. Follow one request from Enter to the answer
in [How a turn works](docs/how-a-turn-works.md).

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

The [`Provider`](crates/harness-provider/src/lib.rs) trait hides which API the
model speaks. Two clients implement it.
[`ChatClient`](crates/harness-provider/src/client.rs) speaks the OpenAI-style
chat API: the hosted service, OpenRouter, OpenAI, DeepSeek, and local servers
such as Ollama. [`AnthropicClient`](crates/harness-provider/src/anthropic.rs)
speaks Anthropic's own Messages API. `Agent::new` picks one from the
`dialect` setting. A provider with yet another format would add a third
implementation.

The hosted service (accounts, billing, and the model proxy) is a separate
private service. Nothing in this repository needs it, and connecting with
your own key or a local model never talks to it. Ready-made downloads are
attached to this repository's
[releases](https://github.com/bmtai-projects/HiveMind/releases).

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
