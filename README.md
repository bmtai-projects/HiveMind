# HiveMind

A fast, cost-optimized coding agent — a from-scratch Rust harness in the
shape of [grok-build](https://x.ai/cli), built around one goal: **make an
agentic coding loop cheap enough to give away.** The CLI binary is called
`hivemind`; you run it with `hivemind activate`.

Seven models are selectable with `--model` / `/model`: `hivemind` — the
cheap, fast default — plus six third-party coding models (`claude-sonnet-5`,
`gpt-5.3-codex`, `gemini-3.1-pro`, `grok-build`, `qwen3-coder-plus`,
`kimi-k2-code`) resold through the hosted proxy. BYOK users point their own
key at whatever their provider supports.

> **`hivemind` is a brand alias, not a passthrough.** Which upstream model
> serves it is infrastructure, resolved server-side and deliberately not
> exposed client-side — the proxy rewrites every stream chunk's `model`
> field back to the alias (see `HiveMind-server/src/proxy/upstream.ts`), and
> the agent's own `IDENTITY` prompt tells it to say so plainly rather than
> guess. Naming the upstream vendor in user-facing copy undoes that on
> purpose-built work, so don't.

> **This repo is source-only and private.** Compiled binaries are published
> to the public [`HiveMind-releases`](https://github.com/BibhabenduMukherjee/HiveMind-releases)
> repo, which has no source in it — that's where the public install command
> lives. See [Distributing a release](#distributing-a-release) for how the
> two repos connect.

## Why it's cheap

Model choice isn't what makes Cursor/Claude expensive — **re-sent context**
is. Every turn of an agent loop resends the whole growing conversation, and
by mid-session that's tens of thousands of tokens billed on every call. Two
things close that gap almost entirely:

1. **Prompt-prefix caching.** Cache-hit prompt tokens bill at roughly
   **1/50th** the cache-miss rate. Keep the prefix (system prompt, tool
   schemas) byte-stable turn to turn and most of a session's input tokens
   land in that discount. Most providers do this automatically; the ones
   that need an explicit breakpoint are flagged per-model by
   `needs_explicit_cache_control` in the catalog.
2. **Compaction.** Once a session's usage crosses a threshold, fold older
   turns into one summary instead of re-sending (and re-billing) them
   forever.

On the default model that puts a full coding session in cents, not dollars.
Real per-model rates live in `KNOWN_MODELS`
([`harness-config/src/lib.rs`](crates/harness-config/src/lib.rs)) — the
single source of truth, so quoting numbers here would just rot. See
[Optimizations](#optimizations-implemented) for what's actually wired up.

## Quick start

### Install

Public install command (no source access needed — this is what goes in
user-facing docs):

```sh
curl -fsSL https://raw.githubusercontent.com/BibhabenduMukherjee/HiveMind-releases/main/install.sh | bash
```

If you have access to this (private) repo, build from source instead:

```sh
git clone https://github.com/BibhabenduMukherjee/HiveMind.git
cd HiveMind
cargo build --release -p harness-cli
./target/release/hivemind --version
```

### Run

```sh
hivemind auth login                            # hosted: no provider key of your own
# or, BYOK:
export HIVEMIND_API_KEY=sk-...                 # $DEEPSEEK_API_KEY still works (legacy)

hivemind activate                              # interactive REPL, on the default model
hivemind activate -p "summarize src/main.rs"   # headless one-shot
hivemind activate --model claude-sonnet-5      # start on a specific model
hivemind activate --continue                   # resume the last session here
hivemind models                                # what's selectable
```

No config file is required. To customize models, thresholds, or a proxy
`base_url`, copy [`config.example.toml`](config.example.toml) to
`~/.config/hivemind/config.toml`.

## Optimizations implemented

Every one of these is real, wired-up behavior — not a roadmap item:

| Optimization | Where | Effect |
|---|---|---|
| **Prefix-stable requests + cache-hit visibility** | [`harness-cli/src/ui.rs`](crates/harness-cli/src/ui.rs), [`harness-provider/src/wire.rs`](crates/harness-provider/src/wire.rs) | Tool schemas serialize in sorted, deterministic order; the provider's `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` are parsed and shown live (`cache 92%`) so the win is visible, not assumed. |
| **Context compaction** | [`harness-agent/src/compaction.rs`](crates/harness-agent/src/compaction.rs) | At `compaction_threshold_percent` (default 75%) of the context window, older turns are folded into one model-generated summary via a cheap model call — never silently truncated, never re-billed forever. |
| **Anchored edits over full rewrites** | [`harness-tools/src/edit.rs`](crates/harness-tools/src/edit.rs) | `edit_file` replaces just an `old_string`→`new_string` span, so modifying a file emits tens of output tokens instead of re-emitting the whole thing. Output is the priciest token class (never cached), which makes this the largest single lever on a coding session's cost — and it can't corrupt untouched code, so fewer botched edits means fewer retry turns and less escalation. |
| **Auto-escalation off the cheap default** | [`harness-agent/src/agent.rs`](crates/harness-agent/src/agent.rs) | Every task starts on the cheap default. If the model repeats an identical tool call or hits repeated tool errors (a doom-loop symptom), the harness escalates to `escalate_to_model` for that task only, then resets on the next input. Scoped to rescuing the default: it never fires once a user has explicitly picked a model, since that could be a much bigger cost jump than they expect. |
| **Parallel tool dispatch** | [`harness-tools/src/tool.rs`](crates/harness-tools/src/tool.rs) | Multiple tool calls in one turn run concurrently via `tokio::JoinSet`, then are reassembled in original call order — concurrent latency, deterministic transcript. |
| **Retry/backoff with jitter** | [`harness-provider/src/retry.rs`](crates/harness-provider/src/retry.rs) | 429/5xx/network errors retry with exponential backoff + jitter, honoring a server's `Retry-After` header, surfaced to the UI via a retry hook. |
| **Connection reuse** | [`harness-provider/src/client.rs`](crates/harness-provider/src/client.rs) | One pooled `reqwest::Client` per process — every request, retry, and background summarization call reuses keep-alive HTTP connections. |
| **Zero-clone request path** | [`harness-provider/src/client.rs`](crates/harness-provider/src/client.rs) | The provider borrows the conversation only long enough to serialize it; sending a turn never clones the (potentially large) message history. Retries resend a cheaply-refcounted `Bytes` body, not a re-copy. |
| **Live cost readout** | [`harness-cli/src/ui.rs`](crates/harness-cli/src/ui.rs) | Every response line shows `$turn / $session` cost, computed from real usage × that model's pricing — the point of all of the above is a number you can watch stay small. |

## Architecture

```
crates/
  harness-types      provider-neutral wire model (Message, ToolCall, Usage, StreamEvent)
  harness-config     config.toml + env resolution: model catalog, keys, policy
  harness-provider   the streaming client: SSE decode, retries, connection reuse
  harness-tools      the Tool trait, registry, parallel dispatch, fs + shell builtins
  harness-agent      the sample<->tools loop: compaction, tiering/escalation, doom-loop guard
  harness-cli        the `hivemind` binary (activate subcommand): clap args, REPL/headless, terminal UI, cost display
```

Six small crates instead of grok-build's ~70 — same layering, deliberately
compact. Data flows one way: `harness-cli` builds a `Registry` (tools) and a
`Resolved` config, hands both to `harness-agent::Agent`, which drives
`harness-provider` and streams events back through a UI trait the CLI
implements. No crate reaches back up the stack.

### Provider abstraction, kept honest

There's no `Provider` trait today — `Agent` is concretely typed against a
single client, on purpose. Every model in the catalog is reached over the
same OpenAI-compatible Chat Completions dialect (hosted models via the
proxy, BYOK straight to the vendor), so one client covers all of them and a
trait would be an abstraction with one implementation. A provider speaking a
genuinely different wire format means: define its dialect in a new module
(mirroring [`harness-provider/src/wire.rs`](crates/harness-provider/src/wire.rs)),
then introduce the trait `Agent` needs at that point. Not before.

The client type is still named `DeepSeekClient` — a historical name from
when that was the only backend, not a statement of scope. It speaks the
generic dialect described above.

## Tools

Seven built-ins, all workspace-confined (`--workdir`, default `.`):

- `read_file`, `write_file`, `list_dir` — path-escape-checked against the
  workspace root.
- `edit_file` — exact `old_string`→`new_string` replacement in an existing
  file. The model rewrites only the changed span instead of re-emitting the
  whole file, so a one-line change costs tens of output tokens, not thousands
  — and can't corrupt the parts it never re-typed. The system prompt steers
  modifications here; `write_file` is for creating new files.
- `search` — read-only, workspace-confined literal content search returning
  `path:line: text` hits. Needs no approval (it only reads), so the model
  locates code in one cheap turn without a shell round-trip or a `grep`/`rg`
  dependency, skipping `.git`/`target`/`node_modules` and large/binary files.
- `semantic_search` — ranked *similarity* search (the retrieval half of a
  "modern agent"): the repo is chunked and embedded into an incrementally
  cached vector index, the query is embedded, and the closest chunks come back
  as `path:startLine-endLine` ranges. Lets the model find code by concept, not
  just exact string. The default embedder is local and dependency-free
  (feature-hashing — lexical/fuzzy, ships lean); it's behind an `Embedder`
  trait so a real neural model (e.g. `fastembed`/BGE) drops in without
  touching the index or tool. See [`semantic.rs`](crates/harness-tools/src/semantic.rs).
- `run_shell` — gated by an interactive `[y/N]` approval prompt by default;
  `--yolo` or headless (`-p`) mode auto-approves. Runs under a timeout with
  `kill_on_drop` so a cancelled/timed-out command can't orphan a process.

## Flags

All of these are flags on `hivemind activate`, e.g.
`hivemind activate --model claude-sonnet-5`. `hivemind activate --help` is
the authoritative list; this table is a summary.

| Flag | Meaning |
|---|---|
| `-p, --prompt` | Run one prompt headlessly (auto-approves shell), then exit. |
| `--workdir` | Workspace root. Default `.`. |
| `--config` | Config file path. Default `~/.config/hivemind/config.toml`. |
| `--model` | Model to start on. `hivemind models` lists what's selectable. |
| `--api-key` / `--base-url` | Override resolved endpoint (e.g. point at a proxy or local mock). |
| `--yolo` | Auto-approve all shell commands. Off by default. |
| `--reasoning-effort` | Thinking depth on models that support it. Opt-in: slower and pricier. |
| `--show-reasoning` | Also dump the raw reasoning text, on models that emit it. |
| `--budget` | Hard USD cap for the session. Stops at a turn boundary, never mid-edit. |
| `--mode` | `standard` (default, local and free) or `pro` (hosted embeddings, billed). |
| `--continue` | Resume the most recent session for this workspace. |
| `--resume <ID>` | Resume one specific session (see `hivemind sessions`). |
| `--protocol json` | Speak NDJSON on stdin/stdout instead of the REPL — what the VS Code extension uses. |

## Development

```sh
cargo check -p <crate>              # target one crate; faster than a full build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Distributing a release

```sh
git tag v0.1.0
git push origin v0.1.0              # triggers .github/workflows/release.yml
```

Builds macOS (x86_64/aarch64), Linux (x86_64/aarch64), and Windows (x86_64)
binaries, then **publishes them to the public `HiveMind-releases` repo**, not
this one — see `.github/workflows/release.yml`. That cross-repo publish
needs a secret this repo doesn't manage automatically:

- **`RELEASES_REPO_TOKEN`** — a PAT with `Contents: Read and write` scoped
  to `BibhabenduMukherjee/HiveMind-releases`, added under this repo's
  *Settings > Secrets and variables > Actions*. Without it, the `release`
  job's publish step fails with a permissions error — the default
  `GITHUB_TOKEN` can't write to a different repo.

`HiveMind-releases/install.sh` downloads whichever asset matches the
caller's platform. Both the workflow and the installer have been run
end-to-end against real GitHub infrastructure, not just written and hoped.

## Roadmap

Scoped out of this pass on purpose — natural next additions, each behind an
existing seam:

- **A second provider** (OpenAI/Anthropic/xAI) — see [Provider abstraction](#provider-abstraction-kept-honest).
- **Undo stack for edits** — `edit_file` (shipped) already scopes each change to a span; a per-session undo/redo over file writes is the natural follow-on.
- **Neural `semantic_search`** — the retrieval pipeline (chunk/index/cosine/cache) is shipped behind an `Embedder` trait with a lean local default; swapping in a real embedding model (`fastembed`/BGE locally, or a hosted embeddings API) behind a cargo feature makes it truly semantic. Persist the index to disk to skip the cold-start rebuild.
- **MCP client** — mount external tool servers.
- **Session persistence** — `Agent::history()` already exposes the full transcript; save/resume is a serialization layer away.
- **Explicit `anthropic-style` cache breakpoints** — wired up and gated per-model by `needs_explicit_cache_control`; most models cache automatically and must not be sent it.
- **A `/cost` and `/tier` REPL command** — the pricing and tier machinery already exists in `harness-config`/`harness-cli/src/ui.rs`; this is UI wiring, not new logic.

## License

MIT — see [LICENSE](LICENSE).
