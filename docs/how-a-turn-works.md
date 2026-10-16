# How a turn works

You press Enter after typing "add a test for this function."
HiveMind sends that request to a model. The model asks to read code,
adds a test, runs it, and explains the result.

This guide follows one possible path through an interactive session.
The model chooses the tools and their arguments. The sequence below is an
example, not a script that HiveMind always follows.

A **request** is one user message and all the work needed to answer it.
A **turn** here is one trip to the model, followed by any tools it asks for.
One request can take several turns.

## 1. Start the session and accept the message

In [`crates/harness-cli/src/main.rs`](../crates/harness-cli/src/main.rs),
`run` loads settings and resolves the working folder.
It creates a `Registry`, the collection of tools the model can use.
It also creates `ChatClient`, which talks to the model, and `TermUi`,
which displays progress in the terminal. It passes these to `Agent::new`.

The same file's `system_prompt` builds the **system prompt**: instructions
that apply to the whole session. Project conventions are loaded once at
startup. They do not get read again before each turn.

The `repl` function waits for input with the line editor's `read_line`.
It handles slash commands separately. For ordinary text, it expands file
mentions such as `@path` and calls `run_steerable`.
That function drives `Agent::run` and lets Ctrl+C pause for new instructions.

Our example has no file mention. The text reaches the agent as a user message.

## 2. Prepare the conversation

In [`crates/harness-agent/src/agent.rs`](../crates/harness-agent/src/agent.rs),
`Agent::run` adds the user message to the conversation.
It opens a checkpoint, a record used to restore file edits with `/undo`.
It resets the active model to the session's default model for this request.

At the start of each turn, it checks the spending budget.
Then `Agent::trim_if_needed` can remove old tool results to reduce input
without making a model call. It protects the current request's results.

Next, `Agent::compact_if_needed` calls `maybe_compact` in
[`crates/harness-agent/src/compaction.rs`](../crates/harness-agent/src/compaction.rs).
**Compaction** means folding older messages into one summary.
It happens when the conversation reaches the configured threshold.
The default is 75% of the model's **context window**, the amount of text
the model can handle in one request.

Compaction keeps the system prompt and recent messages.
The summary carries earlier intent, constraints, and decisions.
The code also adds a list of edited files.
Making the summary costs a model call. Later turns save money because they
send the shorter summary instead of all those older messages.
A short, fresh session usually skips this step.

`Agent::run` builds a `ChatRequest` with the model, conversation, and tool
schemas. A **schema** describes a tool's name and accepted arguments.

This is where the stable prompt start matters.
`Registry::schemas` in
[`crates/harness-tools/src/tool.rs`](../crates/harness-tools/src/tool.rs)
returns tools in name order. The system prompt also stays stable.
Providers that support **prompt caching** can reuse this unchanged start
and charge less for its input. A cache hit is reuse of previously sent text.
Changing the advertised tools or instructions can change that start.

For models that need an explicit cache marker, `Agent::run` sets
`cache_prompt_prefix`. The `to_wire_messages` function in
[`crates/harness-provider/src/wire.rs`](../crates/harness-provider/src/wire.rs)
adds the marker to the system message.

## 3. Send the request and show the stream

`Agent::run` calls `Ui::turn_started`, then `ChatClient::stream` in
[`crates/harness-provider/src/client.rs`](../crates/harness-provider/src/client.rs).

`ChatClient::stream` encodes the request as JSON, a text format for structured
data. It starts a background task and returns a channel that delivers events
to the agent. It encodes the request once, so retries reuse the same bytes.

In that file, `ChatClient::run_with_retries` handles temporary failures.
`ChatClient::try_once` sends the request to `/chat/completions`.
`ChatClient::decode_stream` reads server-sent events: small pieces of the
response sent over the open connection. It collects text and tool arguments.
It emits text fragments, tool-start notices, and a final `Done` event.

Back in `agent.rs`, `Agent::drain_stream` receives these events.
Text goes straight to `Ui::assistant_delta`.
A tool-start notice goes to `Ui::tool_call_pending`.
The final event supplies the full response, including tool calls and usage.

The `Ui` trait in
[`crates/harness-agent/src/ui.rs`](../crates/harness-agent/src/ui.rs)
is an interface for reporting progress. The agent does not draw the terminal
itself. `TermUi` implements this interface in
[`crates/harness-cli/src/ui.rs`](../crates/harness-cli/src/ui.rs).
Its `turn_started` shows the thinking indicator.
Its `assistant_delta` prints text as it arrives.
Its `tool_call_pending` shows which tool is being requested.

In our example, the model asks for `read_file`.
A **tool call** contains an ID, a tool name, and arguments.
It describes work to perform; receiving it does not read the file yet.
The agent waits for the completed response before dispatching tools.

## 4. Read the function

`Agent::run` records the assistant response, then calls
`Agent::dispatch_and_record` in `agent.rs`.
That function captures files before edits so undo can restore them.
It reports tool starts and checks any configured hooks.
A **hook** is a user-configured command that can inspect or block a tool call.

Allowed calls reach `Registry::dispatch_many` in `tool.rs`.
It finds each tool by name and calls its `execute` method.
Independent groups run concurrently. Calls with the same conflict key run
in arrival order. A conflict key identifies a shared resource, such as a file.
Results come back in the original call order even if tools finish out of order.
Unknown tools and tool failures become error results the model can read.

For `read_file`, the implementation is `ReadFile::execute` in
[`crates/harness-tools/src/fs.rs`](../crates/harness-tools/src/fs.rs).
It resolves the path inside the workspace and reads the file.
It returns the requested range and records the file's contents for later
checks that it has not changed.

`Agent::dispatch_and_record` reports completion through `Ui::tool_end`.
It appends a tool-result message with the call's ID.
That ID pairs the result with the model's request to read the file.
`Agent::run` saves the conversation through `Agent::persist`,
then starts another turn. The next model request includes the file contents.

## 5. Add the test

With the function and existing tests in view, the model asks for `edit_file`.
The same stream and dispatch path runs again.

`EditFile::execute` in
[`crates/harness-tools/src/edit.rs`](../crates/harness-tools/src/edit.rs)
reads the file and checks whether it changed since the last read.
It replaces an exact old string with the new string supplied by the model.
An ambiguous match fails unless the model explicitly requests all matches.
It writes the edited file and returns a small view of the changed area.

This saves output cost: the model sends a replacement for one piece of text,
rather than generating the whole file again.
The returned view can also avoid a separate read before the next edit.

The agent records the edit result and sends it back on the next turn.
The model can see whether the edit succeeded.

## 6. Run the test and keep large output small

Now the model asks for `run_shell`, with a test command such as `cargo test`.
`Bash::execute` in
[`crates/harness-tools/src/bash.rs`](../crates/harness-tools/src/bash.rs)
implements this tool. In a normal interactive session, it asks for approval.
The callback is `terminal_approve` in `crates/harness-cli/src/ui.rs`.
If you approve, the command runs in the workspace.
The result includes its exit status and output. A denial is returned too.

The tool result passes through `Agent::dispatch_and_record`.
For a large result, `Agent::offload_if_large` stores the full text on disk.
It uses `ArtifactStore::store` and `preview` in
[`crates/harness-tools/src/artifact.rs`](../crates/harness-tools/src/artifact.rs).

An **artifact** is a saved tool result. The conversation gets a short preview
and a handle, an address the `read_artifact` tool can use to fetch a range.
The model gets useful output without paying to send the entire log again
on every later turn. If storage fails, the agent falls back to the tool summary.
This happens after the UI has received the tool's result.

The next turn sends the test result to the model.
If the test fails, the model can read more, edit again, and rerun it.
These are more passes through the same loop.

## 7. Return the answer

After the test passes, the model replies with what changed and what it checked.
`Agent::drain_stream` forwards this text as it arrives.
`TermUi::assistant_delta` prints it, and `TermUi::assistant_done` ends the line.

After each model response, `Agent::run` updates usage and estimated cost.
`TermUi::usage` shows input and output token counts, cache hits when reported,
and costs when the model has known pricing.
A **token** is a small unit of text that providers count for usage and billing.

A response without tool calls is the normal end of the request.
Before returning, `Agent::run` checks its record of edits and commands.
If validation is missing, it can ask the model once to verify its work.
This is a reminder, not a guarantee that tests ran or passed.

Finally, the agent saves the checkpoint and conversation.
Control returns to `repl`, which waits for the next message.

The central path is the same throughout:
user message, model response, tool calls, tool results, then another model
response. The CLI accepts input and draws progress. The agent owns the loop.
The provider moves messages over the network. The tools do the local work.
