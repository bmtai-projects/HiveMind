# Done: turn-level undo (`/undo`)

Shipped in commit `675c5f0` (v0.4.2). Ported from grok-build's rewind system,
scoped down to in-memory-only, no redo, no shell-tool coverage, REPL-only —
see git history for the full original design writeup.

Re-validated fresh before writing this update: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test
--workspace` (all green, including the 10-test `harness-agent` checkpoint
suite), `cargo build --workspace --release --locked` — all pass against the
current `main`. Live end-to-end validation (real `Agent` + real
`DeepSeekClient` + real file I/O against a local fake model server, both
edit→undo and create→undo) was performed when the feature was built and the
checkpoint code hasn't changed since.

---

# Next feature: tool hooks (`PreToolUse` veto), ported from grok-build's hook system

Per the earlier comparison in this file's history, hooks was the clear
runner-up to checkpoints: it closes a real safety gap checkpoints
deliberately don't cover (checkpoints only undo file edits *after* the fact,
and never cover `run_shell` at all — grok-build's own scoping choice, which
HiveMind's checkpoint feature matches). A `PreToolUse` hook can block a
dangerous `run_shell` call *before* it ever executes, which is a materially
different and stronger safety property.

## What grok-build actually does (confirmed by reading the real source)

`crates/codegen/xai-grok-hooks` in grok-build, 7,613 lines across:

| File | Lines | What it does |
|---|---|---|
| `event.rs` | 545 | 14 event types (session lifecycle, tool, subagent, compaction, notification); JSON envelope shape |
| `config.rs` | 1,293 | Hook spec parsing, matcher config, per-hook timeout/env |
| `discovery.rs` | 996 | Multi-location hook auto-discovery (project/user/plugin dirs) + precedence layering |
| `dispatcher.rs` | 895 | Fan-out to matching hooks, aggregates blocking decisions |
| `env_expand.rs` | 856 | `${VAR}` substitution in hook commands/URLs, with secret-redaction for display |
| `runner/http.rs` | 1,012 | HTTP-handler-type hooks (POST to a URL instead of spawning a process) |
| `runner/command.rs` | 1,272 | Subprocess handler: spawn, write JSON envelope to stdin, timeout, parse result |
| `trust.rs` | 171 | SSRF allowlist for HTTP hooks |
| `matcher.rs` | 217 | Tool-name/glob matching for which hooks fire on which calls |
| `result.rs` | 70 | `HookDecision::Allow \| Deny{reason, hook_name}`; fail-open on crash/timeout |

**The actual execution contract** (`runner/command.rs`, confirmed by reading
it, not guessed): the event envelope is JSON on the child process's stdin.
The hook's outcome is read from **either** structured JSON on stdout
(`{"decision": "...", "reason": "..."}`) **or**, as a fallback, the exit
code: `0` = allow, `2` = deny (`DENY_EXIT_CODE`, deliberately matching Claude
Code's own hook convention so a script can plausibly be reused across
tools), anything else = the hook itself failed. A failed/crashed/timed-out
hook is **fail-open** — `HookRunResult::Failed`, explicitly documented as
not blocking the agent. This fail-open behavior is a real, deliberate safety
property (a broken hook script must never be able to wedge the agent) and
is being ported as-is, not reconsidered.

`PostToolUse` hooks are observational only — `result.rs`'s own doc comment
scopes `HookDecision` to "the outcome of a **blocking** (`pre_tool_use`)
hook dispatch," meaning post-hooks never produce a decision to act on, only
a success/fail outcome to log. This asymmetry is grok-build's own design,
carried over rather than inventing veto-after-the-fact semantics that
wouldn't mean anything (the tool already ran).

## HiveMind design (scoped down, matching the "1-2 days minimal" estimate)

**Cut, on purpose:**
- Only `PreToolUse` and `PostToolUse` — not the other 12 event types
  (session lifecycle, subagent, compaction, notifications). Nothing in
  HiveMind's `Agent` currently models those moments as discrete events, and
  none of them carry the safety motivation `PreToolUse` does.
- Command handler only — **no HTTP handler type**. This alone cuts
  `runner/http.rs` (1,012 lines) and `trust.rs`'s SSRF allowlist (171 lines)
  entirely; a use case that genuinely needs an HTTP callback can shell out
  to `curl` from a command hook.
- No `${VAR}` templating/expansion (`env_expand.rs`, 856 lines) — a hook
  command is a plain string passed to the same shell-execution path
  `run_shell` already uses; if a script needs a secret, it reads its own
  process environment normally, no HiveMind-side substitution layer.
- No multi-location auto-discovery (`discovery.rs`, 996 lines) — hooks are
  declared directly in `config.toml`'s new `[[hooks]]` array, one place, no
  plugin/project/user precedence stack.
- Matching is exact tool name or `*` (all tools) — HiveMind has exactly 7
  tool names total, so `matcher.rs`'s glob/regex machinery (217 lines) has
  no real problem to solve here.
- Fail-open and the exit-code-2-deny convention are **kept as-is** (see
  above — these aren't scope cuts, they're the load-bearing safety/
  compatibility properties).

**Data model** (new `crates/harness-agent/src/hooks.rs`, sibling to
`checkpoint.rs`):

```rust
pub enum HookEvent { PreToolUse, PostToolUse }

pub struct HookSpec {
    pub name: String,
    pub event: HookEvent,
    pub matcher: Option<Vec<String>>, // tool names this hook applies to; None = all
    pub command: String,              // spawned the same way run_shell spawns
    pub timeout_ms: u64,              // default 5000
}

pub enum HookDecision {
    Allow,
    Deny { reason: String, hook_name: String },
}
```

**Config wiring:** new `[[hooks]]` array in `config.toml`, parsed in
`harness-config` alongside the existing `[model]`/`[agent]` sections —
same TOML file, same parsing pattern, no new config mechanism.

**Envelope on stdin:** a minimal HiveMind-specific JSON shape (event name,
tool name, tool args, workspace root) — not grok-build's full metadata
envelope. Reuse their 128 KB truncation constant for tool args/results
(`event.rs`'s `MAX_PAYLOAD_SIZE`) as a sane default against a giant
`write_file` content blowing up a hook's stdin.

**Execution point:** `Agent::dispatch_and_record` in `agent.rs`, which
already does `checkpoint.capture(...)` before `self.tools.dispatch_many
(calls)`. For each call: run matching `PreToolUse` hooks first; on `Deny`,
skip dispatching *that* call and synthesize a tool-result message carrying
the denial reason (the same shape a real tool error already takes), so the
model sees "blocked: \<reason\>" and can react instead of the turn silently
losing a call. After dispatch, fire matching `PostToolUse` hooks per call,
observationally (log success/fail, no effect on the result already
returned to the model) — matching grok-build's own asymmetry.

## Test plan

**Unit tests, `crates/harness-agent/src/hooks.rs`:**
1. Matcher: hook with `matcher: Some(["run_shell"])` fires only for that
   tool; `None` fires for all; a non-matching call is untouched.
2. Exit code `0` → `Allow`; exit code `2` → `Deny` with a synthesized
   reason; any other exit code → treated as failed, not denied.
3. Structured JSON on stdout (`{"decision":"deny","reason":"..."}`) takes
   precedence over the exit-code fallback.
4. Timeout and a crashing command both fail open (`Allow`), not panic and
   not block.
5. A `Deny` on `PreToolUse` prevents `dispatch_many` from ever being called
   for that specific tool call, and the resulting message the model sees is
   a tool-result containing the denial reason, not a crash or a silently
   dropped call.
6. `PostToolUse` hook outcome never changes what was already returned to
   the model, even if the hook itself fails.

**Live end-to-end validation:** build the release binary; write a real
`config.toml` with a `[[hooks]]` entry whose command is a small script that
denies (`exit 2`) any `run_shell` call whose args contain `rm -rf`; run a
real REPL session (or the same fake-model-server pattern used for `/undo`'s
validation) that prompts exactly that dangerous command; confirm — checking
independently, not trusting the CLI's own output — that the shell command
never actually ran, and that the model's next turn shows it received the
denial reason.

## Verification gates before calling this done

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, `cargo build --workspace --release
--locked`, all green — plus the live end-to-end step above, actually run.
