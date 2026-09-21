use std::sync::{Arc, Mutex};
const MAX_INTERJECTION_CHARS: usize = 25_000;
#[derive(Clone, Default)]
pub struct InterjectionQueue {
    pending: Arc<Mutex<Vec<String>>>,
}

impl InterjectionQueue {
    pub fn new() -> Self {
        Self::default()
    }

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
