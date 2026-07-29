//! Mid-turn steering: messages the user sends *while* the agent is already
//! working.
//!
//! Without this, a long multi-step turn is all-or-nothing — watching it head
//! the wrong way at step 5 leaves only "kill it and start over", which throws
//! away every tool result already paid for. A queued interjection instead
//! reaches the model at the next turn boundary, so the work so far survives
//! and the model gets to weigh the new instruction against what it was
//! already doing.
//!
//! Delivery is deliberately *not* immediate. [`crate::Agent`] drains this at
//! the top of its turn loop — the same single safe point budget enforcement
//! uses — never mid-stream and never between an assistant's `tool_calls` and
//! their results (which would be an invalid transcript, rejected outright by
//! an OpenAI-dialect API).

use std::sync::{Arc, Mutex};

/// Per-message ceiling before truncation. A pasted stack trace or file dump
/// shouldn't be able to blow out the context window from a side channel that
/// bypasses the normal input path.
const MAX_INTERJECTION_CHARS: usize = 25_000;

/// A cheaply-cloneable handle to one agent's pending interjections. Clones
/// share the same queue, so a host can hand one to an input task and keep
/// another for itself.
#[derive(Clone, Default)]
pub struct InterjectionQueue {
    pending: Arc<Mutex<Vec<String>>>,
}

impl InterjectionQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a message for delivery at the next turn boundary. Returns
    /// `false` for blank input (nothing queued) so a caller can tell an
    /// accidental bare Enter from a real instruction — the CLI uses exactly
    /// that to distinguish "interject" from "abort".
    pub fn push(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        if text.trim().is_empty() {
            return false;
        }
        self.lock().push(text);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Take everything pending and render it as a single user message, or
    /// `None` when nothing is queued. One message rather than one per
    /// interjection: the framing sentence only needs saying once, and fewer
    /// history entries keeps the transcript (and every future prompt that
    /// replays it) smaller.
    pub fn drain_formatted(&self) -> Option<String> {
        let drained: Vec<String> = self.lock().drain(..).collect();
        if drained.is_empty() {
            return None;
        }
        let header = if drained.len() == 1 {
            "The user sent a message while you were working:".to_string()
        } else {
            format!(
                "The user sent {} messages while you were working:",
                drained.len()
            )
        };
        let body = drained
            .iter()
            .map(|t| format!("<user_query>\n{}\n</user_query>", truncate(t)))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!("{header}\n{body}"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        // Poisoning would mean a panic while holding the lock; the queue is
        // plain data with no invariant to corrupt, so recovering is strictly
        // better than propagating a panic into the agent loop.
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Truncate on a char boundary — slicing raw bytes would panic on any
/// multi-byte character straddling the cutoff.
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_INTERJECTION_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_INTERJECTION_CHARS).collect();
    format!("{head}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_queue_yields_nothing() {
        let q = InterjectionQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.drain_formatted(), None);
    }

    #[test]
    fn blank_input_is_rejected_and_never_queued() {
        let q = InterjectionQueue::new();
        assert!(!q.push("   \n  "));
        assert!(!q.push(""));
        assert!(q.is_empty(), "blank input must not become an interjection");
    }

    #[test]
    fn a_single_interjection_uses_the_singular_framing() {
        let q = InterjectionQueue::new();
        assert!(q.push("stop and fix the failing test first"));
        let out = q.drain_formatted().unwrap();
        assert!(out.starts_with("The user sent a message while you were working:\n"));
        assert!(out.contains("<user_query>\nstop and fix the failing test first\n</user_query>"));
    }

    #[test]
    fn several_interjections_merge_into_one_message_in_order() {
        let q = InterjectionQueue::new();
        q.push("first");
        q.push("second");
        let out = q.drain_formatted().unwrap();
        assert!(out.starts_with("The user sent 2 messages while you were working:"));
        let first = out.find("first").expect("first present");
        let second = out.find("second").expect("second present");
        assert!(first < second, "order must be preserved: {out}");
    }

    #[test]
    fn draining_empties_the_queue_so_nothing_is_delivered_twice() {
        let q = InterjectionQueue::new();
        q.push("only once");
        assert!(q.drain_formatted().is_some());
        assert!(q.is_empty());
        assert_eq!(q.drain_formatted(), None);
    }

    #[test]
    fn clones_share_one_queue() {
        // The whole point of the handle: an input task pushes, the agent drains.
        let a = InterjectionQueue::new();
        let b = a.clone();
        b.push("from the other side");
        assert_eq!(a.len(), 1);
        assert!(a.drain_formatted().unwrap().contains("from the other side"));
        assert!(b.is_empty(), "clones must observe the drain too");
    }

    #[test]
    fn an_oversized_interjection_is_truncated_on_a_char_boundary() {
        // Multi-byte chars: a byte-wise slice at the limit would panic.
        let q = InterjectionQueue::new();
        q.push("é".repeat(MAX_INTERJECTION_CHARS + 500));
        let out = q.drain_formatted().unwrap();
        assert!(out.contains("[truncated]"));
        assert!(out.chars().count() < MAX_INTERJECTION_CHARS + 200);
    }

    #[test]
    fn text_at_exactly_the_limit_is_left_alone() {
        let q = InterjectionQueue::new();
        q.push("a".repeat(MAX_INTERJECTION_CHARS));
        let out = q.drain_formatted().unwrap();
        assert!(!out.contains("[truncated]"));
    }

    #[test]
    fn clear_discards_without_delivering() {
        let q = InterjectionQueue::new();
        q.push("never mind");
        q.clear();
        assert_eq!(q.drain_formatted(), None);
    }
}
