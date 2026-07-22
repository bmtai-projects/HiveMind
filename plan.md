# Next feature: turn-level undo (`/undo`), ported from grok-build's rewind system

Comparison of xAI's `grok-build` (`/Users/soulknower/Documents/projects/grok-build-main`, 62
crates, full TUI + ACP + MCP + sandboxing) against HiveMind (6 crates, REPL + headless
CLI, no TUI) to pick the next feature worth porting. Four candidates were investigated in
grok-build's actual source (not guessed from crate names) before picking one.

## Candidates investigated

| Feature | grok-build crate(s) | Core size (excl. tests/TUI) | TUI-coupled? | Verdict |
|---|---|---|---|---|
| **Checkpoints / rewind** | `xai-grok-workspace` (`checkpoint.rs`, `checkpoint_store.rs`, `file_state.rs`) | ~2,000–2,500 lines in grok-build; a HiveMind-scoped version is far smaller (see below) | No — restore is a plain method call (ACP extension in grok-build); only the `/rewind` picker UI is pager-specific | **Picked** |
| **Hooks** (`PreToolUse` veto, `PostToolUse`, etc.) | `xai-grok-hooks` | ~5,000 lines full parity; minimal (command handlers, 2-3 events, no HTTP/SSRF/Claude-alias layer) is ~1-2 days | No — pure `tokio::process` + JSON stdin, consumed by session/agent logic, not the pager | Strong runner-up, next after this |
| **Self-update** (`grok update`) | `xai-grok-update` | ~6,000 lines full; most of the bulk is Windows locked-exe handling and 3 redundant version-source fallbacks (CDN/npm/gh) | No | Lower priority — infra polish, not agent capability. HiveMind already has the GitHub Releases pipeline; a minimal single-source-plus-atomic-rename version is a half-day port whenever it's wanted |
| **Shell sandboxing** | `xai-grok-sandbox` | ~3,700 lines; wraps an external xAI crate `nono` (Landlock/Seatbelt) + bubblewrap + a hand-rolled seccomp filter | No, but **no Windows support even in grok-build** | Highest long-term security value (especially for `--yolo`), but real risk: `nono` is pinned to an internal xAI crate whose public availability on crates.io is unconfirmed — first step for this one is a spike, not an implementation. Deferred. |

## Why checkpoints, not hooks

Both are well-scoped and genuinely portable. Checkpoints won because:

- It's the direct extension of a value HiveMind already states as a selling point —
  `edit_file`'s own doc comment (`crates/harness-tools/src/edit.rs:14-17`) says edits
  "can't corrupt code it never re-typed." That's per-call safety. Checkpoints add the
  missing per-**turn** safety net: if the model's plan itself was wrong (edited the right
  file the wrong way, or the wrong file entirely), there is currently no way back except
  the user's own `git`. Every comparable agent (Claude Code, Cursor, and grok-build
  itself) treats this as a baseline trust feature, not a nice-to-have.
- It needs zero new dependencies. Hooks need a subprocess/JSON-envelope contract; this
  is pure in-memory Rust operating on data HiveMind already has (`Agent.messages`,
  `Workspace::resolve`).
- It's independently, deterministically testable — no live model, no subprocess, no
  network. Hooks and self-update both need live process/network validation to prove
  end-to-end; sandboxing needs OS-level enforcement testing per platform. Checkpoints
  can be proven correct with pure unit tests plus one live REPL smoke test, fitting a
  single implementation session end-to-end as asked.

Hooks is the clear next pick after this — `PreToolUse` veto (block a dangerous
`run_shell` call before it executes) is a real safety gap checkpoints don't cover, since
checkpoints only undo file edits *after* the fact and explicitly don't cover shell-driven
changes at all (see below — that's grok-build's own scoping choice, not a HiveMind
shortcut).

## What grok-build actually does (informing the design, not copied wholesale)

Confirmed by reading the real source, not inferred from names:

- Checkpointing is **per-turn** (grok-build: per-prompt), not generic tool-call
  middleware. Each mutating tool explicitly emits a `FileWritten{previous_content,
  is_new_file}` notification; a central bridge captures it. **Shell-driven file changes
  are not checkpointed at all** — only the dedicated file-edit tools participate. This
  is grok-build's own considered scope, and it maps directly onto HiveMind: HiveMind
  has exactly two mutating tools, `edit_file` and `write_file`; `run_shell` is out of
  scope for both codebases, for the same reason (a shell command's effects are
  unbounded and can't be captured generically without a much heavier mechanism like a
  shadow git commit of the whole tree).
- What's stored is **full before/after file content** (plain strings, `None` = didn't
  exist), not diffs and not git objects. Git involvement in grok-build is a separate,
  off-by-default, best-effort "domain" alongside the file-content domain — not the
  restore mechanism.
- Restore is a plain method call (`rewind_to(session_id, target_prompt_index)`) exposed
  over grok-build's ACP JSON-RPC layer; the `/rewind` slash command just opens a picker
  UI in the pager that calls it. The restore logic itself has no TUI dependency.
- Default storage is **in-memory only**; a durable JSON-file mirror is opt-in
  (`GROK_WORKSPACE_REWIND_DURABLE=1`) and explicitly documented as "a durability mirror,
  not the restore mechanism." Retention cap: 64 checkpoints per session, oldest evicted.

## HiveMind design (scoped down from the above, not a straight port)

**Explicitly cut, on purpose, to keep this a one-session feature:**
- No disk persistence (matches grok-build's own *default*, not a shortcut relative to it).
- No redo stack — `/undo` only ever moves backward. Re-doing means asking the model again.
- No shell-tool coverage — matches grok-build's own scope, not a gap relative to it.
- No picker UI / no listing command — `/undo [n]` (default 1) walks back sequentially.
  A `/checkpoints` listing command is a natural, cheap follow-up, not built now.
- No conversation-only vs. files-only rewind mode — every checkpoint restores both
  together, always. grok-build's `RewindMode::All` is the sensible single default;
  its `ConversationOnly` variant is a scope cut here.

**Granularity:** one checkpoint per user input to `Agent::run()` (i.e., one REPL line),
covering however many internal model↔tool rounds that input takes to resolve — not one
checkpoint per internal turn and not one per tool call. This matches grok-build's
per-prompt granularity exactly.

**Data model** (new `crates/harness-agent/src/checkpoint.rs`, sibling to the existing
`compaction.rs`):

```rust
struct FileSnapshot { path: PathBuf, before: Option<String> } // None = file didn't exist
struct Checkpoint {
    label: String,              // the user input, truncated, for the /undo confirmation message
    message_len_before: usize,  // Agent.messages.len() before this input was pushed
    files: Vec<FileSnapshot>,   // first-touch-wins within this checkpoint
}
```

**Capture:** in `Agent::dispatch_and_record`, before calling `self.tools.dispatch_many`,
scan the batch for calls named `edit_file` or `write_file` (both already carry a `path`
field in their JSON args — `crates/harness-tools/src/{fs,edit}.rs` — parsed with a
2-field-ignoring local struct, no changes needed to the tool implementations or the
`Tool` trait). For each such path not already captured in the in-progress checkpoint,
read its current content via `Workspace::resolve` + `tokio::fs::read_to_string` (or
record `None` if it doesn't exist) *before* dispatch runs. Snapshotting a file whose
edit then fails validation and never actually writes is harmless — just one wasted read
in the error case — and is the accepted tradeoff against the complexity of only
snapshotting after confirming success under concurrent dispatch.

**Restore, and why "just pop and repeat" is provably correct for `/undo n`:** each
`undo_one()` pops the newest `Checkpoint`, writes back every `before` (or deletes the
file if `before` is `None`), and truncates `Agent.messages` to `message_len_before`.
Calling this in a loop `n` times — not a separate batch/merge code path — already
produces the correct composed result for multi-step undo: each successive truncate can
only shrink `messages` further, so after `n` pops the length is exactly the oldest of
the `n` checkpoints' `message_len_before`; each write to a given path is overwritten by
every subsequent (older) pop that also touched it, so after `n` pops every file sits at
its state from the *oldest* checkpoint among the `n` that touched it. No path→snapshot
merge map needed — proven by the fact that repeated overwrite naturally converges to
the last (oldest) writer.

**Bound:** `const MAX_CHECKPOINTS: usize = 20` (oldest evicted on overflow) — an
in-memory-only design needs a smaller cap than grok-build's 64-per-durable-session,
since large files held as full strings for an entire REPL session's lifetime is the
real memory cost here, not disk space.

**Wiring (no new crate, no new dependency):**
- `Agent` gains one field: `workspace: harness_tools::fs::Workspace` (already `Clone`,
  already does the exact path-escape check needed) and `checkpoints: Vec<Checkpoint>`.
  `Agent::new` gains one parameter for this — the CLI already constructs a `Workspace`
  for the tool registry, so this is passing something that already exists, not building
  something new.
- `crates/harness-cli/src/commands.rs`: new `SlashCommand::Undo(usize)`, parsed the same
  way `/tier`'s optional argument already is; `/undo` added to `COMMAND_NAMES` and
  `HELP_TEXT`.
- `crates/harness-cli/src/main.rs` REPL loop: new match arm calling `agent.undo(n).await`
  and printing the result directly with `println!` — the same pattern already used for
  `/compact` (`agent.force_compact()` + direct print), not routed through the `Ui` trait,
  since like `/compact` this is a REPL-command-initiated action, not an automatic
  in-loop event the way compaction/escalation are.
- Headless one-shot mode (`hivemind activate -p "..."`) does not get an `--undo` flag —
  a single process invocation has nothing to undo *to*, so this is REPL-only by design,
  not an oversight.

## Test plan

**Unit tests, `crates/harness-agent/src/checkpoint.rs`** (no live model, no subprocess):
1. Capture correctly extracts `path` from `edit_file`/`write_file` calls; ignores
   `read_file`/`list_dir`/`run_shell` calls entirely.
2. Restoring a checkpoint whose file existed before → content restored exactly.
3. Restoring a checkpoint whose file did **not** exist before (newly created by the
   agent) → file is deleted on restore.
4. `Agent.messages` truncates back to exactly `message_len_before`.
5. **The core correctness property**: edit file `A` in turn 1, edit it again
   (differently) in turn 2, `undo(2)` → file `A` ends at its turn-1 *pre*-edit content
   (proves the "repeated single pop" composition claim above, not just that undo runs
   without panicking).
6. `MAX_CHECKPOINTS` eviction: pushing past the cap drops the oldest, not the newest.
7. `undo()` against an empty stack is a clean no-op ("nothing to undo"), not a panic.

**Live end-to-end validation** (what "end to end" means for this feature specifically,
since it's a REPL-only interactive feature with no non-interactive equivalent to script
around):
1. Build the real release binary.
2. Start a real `hivemind activate` REPL session against a scratch workspace with one
   seed file of known content.
3. Give it a prompt that edits that file via `edit_file`.
4. Confirm on disk, independently of the CLI's own output, that the file changed.
5. Run `/undo` in the same session.
6. Confirm on disk, independently again, that the file is back to its exact original
   byte content.
7. Repeat once more for the "file didn't exist before" case: prompt it to create a new
   file, confirm it exists on disk, `/undo`, confirm the file is gone.

## Verification gates before calling this done

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo build --workspace --release`, all four green — plus the
live end-to-end steps above, actually run, not assumed.
