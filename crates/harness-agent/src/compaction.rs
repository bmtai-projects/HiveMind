//! Context compaction: once usage crosses a threshold percent of the active
//! model's context window, fold everything except the system prompt and the
//! most recent few turns into one model-generated summary message.
//!
//! This is what lets a long session keep running on the cheap Flash tier
//! instead of either failing outright at the context limit or silently
//! re-sending (and re-billing) an ever-growing transcript.
//!
//! # Why the summary has a fixed shape
//!
//! Compaction is the one place in the harness that deliberately destroys
//! state, so what survives it is a design decision, not a detail. Asking a
//! model for a concise prose summary optimises for the wrong thing: prose
//! that reads well drops whatever is least interesting to *narrate*, and
//! what is least interesting to narrate is usually a flat list of the
//! user's own restrictions -- "don't touch the migration files", "keep this
//! backwards-compatible". Those exist nowhere but in conversation text, and
//! once they're gone the agent will cheerfully violate them for the rest of
//! a session with no sign anything was lost.
//!
//! So the summary is a fixed set of named sections, and a constraint that
//! was never stated has to be written down as "None stated." rather than
//! simply going unmentioned -- an empty slot is visible, a missing sentence
//! is not.
//!
//! # What the harness fills in itself
//!
//! Which files were edited is not a judgment call: it is recorded in the
//! transcript's own tool calls, and the harness can read it exactly. That
//! section is therefore computed here in Rust and appended to whatever the
//! summarizer returns, rather than being asked for -- it cannot be
//! hallucinated, cannot be dropped for brevity, and costs no tokens to
//! produce. The model is asked only for the things that genuinely require
//! reading the conversation: intent, constraints, decisions, dead ends.

use harness_provider::{DeepSeekClient, ProviderError};
use harness_types::{ChatRequest, Message, Role, StreamEvent, Usage};
use serde::Deserialize;

/// Local mirror of `checkpoint`'s helper of the same name. Both exist to
/// pull one field out of a mutating tool's arguments and neither is part of
/// any interface, so they stay separate rather than one importing the
/// other's private type.
#[derive(Deserialize)]
struct PathOnly {
    path: String,
}

pub struct CompactionPolicy {
    pub threshold_percent: u8,
    /// Most-recent messages (after the system prompt) kept verbatim.
    pub keep_recent: usize,
}

pub struct CompactionReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u64,
    /// Usage from the summarization call itself -- this is a real sampled
    /// request against `summarizer_model` and costs real money, distinct
    /// from (and previously invisible next to) the main conversation's
    /// turn-by-turn usage. `None` only when the call failed outright (the
    /// fallback placeholder summary was used), not when the cost was zero.
    pub summary_usage: Option<Usage>,
}

/// Compact `messages` in place if `total_tokens` has crossed the policy
/// threshold relative to `context_window`. Returns `None` when nothing was
/// done (below threshold, or too little history to bother compacting).
///
/// `messages[0]` must be the system prompt — an invariant `Agent` upholds
/// and this function never violates (it only ever drains `1..`).
pub async fn maybe_compact(
    messages: &mut Vec<Message>,
    total_tokens: u64,
    context_window: u64,
    policy: &CompactionPolicy,
    summarizer: &DeepSeekClient,
    summarizer_model: &str,
) -> Option<CompactionReport> {
    if context_window == 0 || total_tokens == 0 {
        return None;
    }
    let percent_used = total_tokens.saturating_mul(100) / context_window;
    if percent_used < policy.threshold_percent as u64 {
        return None;
    }
    if messages.len() <= policy.keep_recent + 1 {
        return None; // not enough history to be worth summarizing
    }

    let messages_before = messages.len();
    let keep_from = compacted_tail_start(messages, policy.keep_recent);

    let old: Vec<Message> = messages.drain(1..keep_from).collect();
    if old.is_empty() {
        return None;
    }

    let transcript = render_for_summary(&old);
    let (summary, summary_usage) = match summarize(summarizer, summarizer_model, &transcript).await
    {
        Ok((summary, usage)) => (summary, Some(usage)),
        Err(_) => (
            "(summary unavailable — a summarization call failed; earlier turns were dropped to free context space)".to_string(),
            None,
        ),
    };

    let summary = with_edited_files(&summary, &old);
    messages.insert(
        1,
        Message::user(format!(
            "<earlier-context-summary>\n{summary}\n</earlier-context-summary>"
        )),
    );

    Some(CompactionReport {
        messages_before,
        messages_after: messages.len(),
        tokens_before: total_tokens,
        summary_usage,
    })
}

/// The index the kept (verbatim) tail should start at: the most recent
/// `keep_recent` messages — but snapped backward so the tail never *begins*
/// on a `tool` message whose requesting assistant turn is in the older span
/// about to be folded into the summary.
///
/// A `tool` message with no immediately preceding `tool_calls` is a hard 400
/// from the (OpenAI-compatible) API, so without this snap a compaction pass
/// can crash the very session it was meant to keep alive — whenever the
/// `keep_recent` boundary happens to fall in the middle of an assistant's
/// tool-call/tool-result group. Pulling the boundary back to include the
/// owning assistant message keeps every kept `tool` message paired.
fn compacted_tail_start(messages: &[Message], keep_recent: usize) -> usize {
    // Clamp into `[1, len-1]` so the system prompt at [0] is always kept and
    // the index below can't run off the end (defensive against keep_recent 0).
    let mut start = messages
        .len()
        .saturating_sub(keep_recent)
        .clamp(1, messages.len().saturating_sub(1));
    while start > 1 && messages[start].role == Role::Tool {
        start -= 1;
    }
    start
}

fn render_for_summary(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        let tag = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if !m.content.is_empty() {
            out.push_str(&format!("[{tag}] {}\n", truncate(&m.content, 800)));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!(
                "[{tag} called tool] {}({})\n",
                tc.name,
                truncate(tc.args.get(), 200)
            ));
        }
    }
    out
}

/// Keeps both ends of `s`, not just the head -- a head-only truncate
/// systematically loses whatever comes later (a file path mentioned near
/// the end of a long tool result, a conclusion after a long chain of
/// reasoning), and the summarizer prompt explicitly asks it to preserve
/// file paths, which this alone can't guarantee but at least stops
/// reliably discarding.
fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let head_len = max_chars * 2 / 3;
    let tail_len = max_chars - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
}

/// Cap on how many paths the computed section lists. A refactor that
/// rewrites two hundred files would otherwise spend more context on the
/// file list than the summary it belongs to -- which is the opposite of
/// what compaction is for.
const MAX_LISTED_FILES: usize = 40;

/// Every workspace path the folded span asked `edit_file`/`write_file` to
/// change, deduplicated and sorted.
///
/// Read from the tool calls themselves, which is the only place this is
/// recorded exactly. One honest limitation: a `Message` keeps a tool's
/// *summary text* but not its status, so a call that was attempted and
/// failed is indistinguishable here from one that succeeded, and both are
/// listed. Over-inclusion is the right way to be wrong -- naming a file the
/// agent tried and failed to write costs a re-read, while omitting one it
/// did write is exactly the silent loss this section exists to prevent.
/// The heading says "asked to edit" rather than "edited" for that reason.
fn edited_paths(old: &[Message]) -> Vec<String> {
    let mut paths: Vec<String> = old
        .iter()
        .flat_map(|m| &m.tool_calls)
        .filter(|tc| crate::validation::MUTATING_TOOLS.contains(&tc.name.as_str()))
        .filter_map(|tc| {
            serde_json::from_str::<PathOnly>(tc.args.get())
                .ok()
                .map(|p| p.path)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Appends the harness-computed file section to a model-written summary.
/// Returns the summary untouched when the folded span edited nothing, so a
/// read-only stretch of conversation doesn't gain an empty heading.
fn with_edited_files(summary: &str, old: &[Message]) -> String {
    let paths = edited_paths(old);
    if paths.is_empty() {
        return summary.to_string();
    }
    let mut out = String::from(summary);
    out.push_str("\n\n## Files asked to edit earlier (recorded by the harness)\n");
    for path in paths.iter().take(MAX_LISTED_FILES) {
        out.push_str(&format!("- {path}\n"));
    }
    if paths.len() > MAX_LISTED_FILES {
        out.push_str(&format!("- …and {} more\n", paths.len() - MAX_LISTED_FILES));
    }
    out
}

/// Asks for named slots rather than a good paragraph.
///
/// "Write None stated." is load-bearing and not politeness: a summarizer
/// told merely to mention constraints has no way to distinguish "there were
/// none" from "I chose not to mention them", and neither does the agent
/// reading the result afterwards. An explicit empty slot is checkable; a
/// missing sentence is not.
const SUMMARY_INSTRUCTIONS: &str = "\
You compress coding-agent conversation history so that work can continue \
after the earlier turns are permanently discarded. Reply with exactly these \
five `## ` sections, in this order, and nothing else:

## Goal
What the user actually asked for, in their own terms.

## Constraints
Every explicit instruction, restriction or preference the user stated -- \
files not to touch, compatibility to preserve, a library, tool or style they \
named. These exist nowhere else once this history is dropped. Write \"None \
stated.\" if there genuinely were none. Never invent or infer one.

## Decisions
Choices already made, and why, so they are not reopened.

## Tried and failed
Approaches already ruled out, and what went wrong, so they are not retried. \
Write \"Nothing yet.\" if none.

## Next steps
What is still outstanding.

Be brief inside each section. Do not add sections, preamble, or closing \
remarks. Do not list edited files -- that is recorded separately.";

async fn summarize(
    client: &DeepSeekClient,
    model: &str,
    transcript: &str,
) -> Result<(String, Usage), ProviderError> {
    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(SUMMARY_INSTRUCTIONS.to_string()),
            Message::user(transcript.to_string()),
        ],
        tools: Vec::new(),
        temperature: None,

        max_tokens: Some(900),
        reasoning_effort: None,
        cache_prompt_prefix: false,
    };
    let mut rx = client.stream(&req);
    let mut result = None;
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Done(resp) = event? {
            result = Some((resp.content, resp.usage));
        }
    }
    result.ok_or(ProviderError::IncompleteStream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_types::ToolCall;

    fn sys() -> Message {
        Message::system("system")
    }
    fn user() -> Message {
        Message::user("hi")
    }
    fn asst_text() -> Message {
        Message::assistant("done")
    }
    fn asst_call() -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read_file".into(),
                args: serde_json::value::RawValue::from_string("{}".into()).unwrap(),
            }],
            tool_call_id: None,
            name: None,
        }
    }
    fn tool_res() -> Message {
        Message::tool_result("c", "read_file", "ok")
    }

    /// An assistant turn calling `tool` once with `args`.
    fn call(tool: &str, args: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: tool.into(),
                args: serde_json::value::RawValue::from_string(args.to_string()).unwrap(),
            }],
            tool_call_id: None,
            name: None,
        }
    }

    fn edits(paths: &[&str]) -> Vec<Message> {
        paths
            .iter()
            .map(|p| call("edit_file", serde_json::json!({"path": p})))
            .collect()
    }

    #[test]
    fn edited_paths_are_deduplicated_and_sorted() {
        let msgs = edits(&["src/b.rs", "src/a.rs", "src/b.rs"]);
        assert_eq!(edited_paths(&msgs), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn both_mutating_tools_are_counted_and_read_only_ones_are_not() {
        let msgs = vec![
            call("write_file", serde_json::json!({"path": "new.rs"})),
            call("edit_file", serde_json::json!({"path": "old.rs"})),
            call("read_file", serde_json::json!({"path": "ignored.rs"})),
            call("search", serde_json::json!({"query": "x"})),
        ];
        assert_eq!(edited_paths(&msgs), vec!["new.rs", "old.rs"]);
    }

    #[test]
    fn the_file_section_is_appended_to_whatever_the_summarizer_returned() {
        let out = with_edited_files("## Goal\nShip it.", &edits(&["src/main.rs"]));
        assert!(out.starts_with("## Goal\nShip it."));
        assert!(out.contains("## Files asked to edit earlier"));
        assert!(out.contains("- src/main.rs"));
    }

    #[test]
    fn a_read_only_span_gains_no_empty_heading() {
        let msgs = vec![asst_call(), tool_res()];
        assert_eq!(
            with_edited_files("## Goal\nExplain it.", &msgs),
            "## Goal\nExplain it."
        );
    }

    /// A mass refactor must not spend more context on its own file list
    /// than on the summary that list belongs to.
    #[test]
    fn a_very_long_file_list_is_capped_and_says_how_many_it_dropped() {
        let owned: Vec<String> = (0..MAX_LISTED_FILES + 5)
            .map(|i| format!("src/f{i:03}.rs"))
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let out = with_edited_files("s", &edits(&refs));
        assert_eq!(out.matches("\n- src/").count(), MAX_LISTED_FILES);
        assert!(out.contains("…and 5 more"));
    }

    /// Malformed or unparseable arguments must not take the rest of the
    /// list down with them -- compaction runs at 75% context, where losing
    /// the summary outright is the expensive failure.
    #[test]
    fn an_unparseable_tool_call_is_skipped_not_fatal() {
        let mut msgs = edits(&["good.rs"]);
        msgs.push(call("edit_file", serde_json::json!({"no_path_here": true})));
        assert_eq!(edited_paths(&msgs), vec!["good.rs"]);
    }

    #[test]
    fn the_instructions_demand_an_explicit_empty_slot_rather_than_silence() {
        assert!(SUMMARY_INSTRUCTIONS.contains("None stated."));
        assert!(SUMMARY_INSTRUCTIONS.contains("Never invent or infer one."));
        // The five slots the agent reads back after a fold.
        for section in [
            "## Goal",
            "## Constraints",
            "## Decisions",
            "## Tried and failed",
            "## Next steps",
        ] {
            assert!(
                SUMMARY_INSTRUCTIONS.contains(section),
                "{section} missing from the summarizer instructions"
            );
        }
    }

    /// The API invariant compaction must never break: every `tool` message is
    /// immediately preceded either by another `tool` message (same response
    /// group) or by an assistant message that actually made tool calls.
    fn tool_pairing_is_valid(messages: &[Message]) -> bool {
        messages.iter().enumerate().all(|(i, m)| {
            if m.role != Role::Tool {
                return true;
            }
            match i.checked_sub(1).map(|j| &messages[j]) {
                Some(prev) => {
                    prev.role == Role::Tool
                        || (prev.role == Role::Assistant && !prev.tool_calls.is_empty())
                }
                None => false, // a tool message at index 0 is always orphaned
            }
        })
    }

    #[test]
    fn snaps_back_over_a_single_orphaned_tool_result() {
        // Boundary (len 13 - keep 8 = 5) lands on a tool result whose
        // assistant parent is at 4 — snapping to 4 keeps them together.
        let m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        assert_eq!(compacted_tail_start(&m, 8), 4);
        assert_eq!(m[4].role, Role::Assistant);
    }

    #[test]
    fn snaps_back_over_a_whole_tool_run() {
        // One assistant turn with three parallel tool calls → three tool
        // results in a row (indices 3,4,5). Boundary at 5 must snap all the
        // way back to the assistant at 2, not stop mid-run.
        let m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            tool_res(),
            tool_res(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
        ];
        assert_eq!(compacted_tail_start(&m, 8), 2);
        assert_eq!(m[2].role, Role::Assistant);
    }

    #[test]
    fn leaves_boundary_untouched_when_tail_starts_on_a_non_tool_message() {
        let m = vec![
            sys(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
        ];
        // Nothing to snap: len 13 - keep 8 = 5, and m[5] is not a tool message.
        assert_eq!(compacted_tail_start(&m, 8), 5);
        assert_ne!(m[5].role, Role::Tool);
    }

    #[test]
    fn the_naive_boundary_would_have_orphaned_a_tool_message() {
        // Documents *why* the snap exists: replicate maybe_compact's slice
        // (drain 1..keep_from, insert a summary at 1) using the OLD boundary
        // and show it produces an invalid transcript.
        let mut m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        let naive_keep_from = (m.len() - 8).max(1); // == 5, lands on a tool result
        m.drain(1..naive_keep_from);
        m.insert(1, Message::user("<summary>"));
        assert!(
            !tool_pairing_is_valid(&m),
            "the naive boundary should leave an orphaned tool message"
        );
    }

    #[test]
    fn slicing_at_the_snapped_boundary_leaves_a_valid_transcript() {
        // Same scenario, now sliced at the snapped boundary exactly as
        // maybe_compact does — the result must satisfy the API pairing rule.
        let mut m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        let keep_from = compacted_tail_start(&m, 8);
        m.drain(1..keep_from);
        m.insert(1, Message::user("<summary>"));
        assert!(
            tool_pairing_is_valid(&m),
            "compaction must never orphan a tool message: {:?}",
            m.iter().map(|x| x.role).collect::<Vec<_>>()
        );
    }
}
