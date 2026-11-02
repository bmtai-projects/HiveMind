//! Leaves file text from earlier requests out of what is sent; the stored transcript keeps it.

use harness_types::{Message, ToolCall};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::validation::MUTATING_TOOLS;

/// Shorter fields are kept: the note costs about as much.
const OMIT_ABOVE_CHARS: usize = 1_000;
const OMITTED_MARKER: &str = "[omitted to save tokens:";
/// Real file text can mention the marker; a copied note is short.
const MAX_NOTE_CHARS: usize = 400;

pub(crate) const COPIED_NOTE_ERROR: &str = "ERROR: this call's text is a note HiveMind left \
in the conversation in place of earlier file contents, not real content. Nothing was written. \
Read the file, then send the actual text.";

/// The conversation to send, or `None` when it goes out unchanged. The request in flight is never touched.
pub(crate) fn lighter_history(
    messages: &[Message],
    current_request_start: usize,
) -> Option<Vec<Message>> {
    let earlier = current_request_start.min(messages.len());
    let mut lighter: Option<Vec<Message>> = None;
    for (i, message) in messages[..earlier].iter().enumerate() {
        for (j, call) in message.tool_calls.iter().enumerate() {
            if let Some(args) = without_file_text(call) {
                lighter.get_or_insert_with(|| messages.to_vec())[i].tool_calls[j].args = args;
            }
        }
    }
    lighter
}

fn without_file_text(call: &ToolCall) -> Option<Box<RawValue>> {
    if !MUTATING_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    let Ok(Value::Object(mut args)) = serde_json::from_str(call.args.get()) else {
        return None;
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("the file")
        .to_string();
    let mut omitted = false;
    for (key, value) in args.iter_mut() {
        let Value::String(text) = value else {
            continue;
        };
        let chars = text.chars().count();
        if key == "path" || chars <= OMIT_ABOVE_CHARS || text.starts_with(OMITTED_MARKER) {
            continue;
        }
        *text = format!(
            "{OMITTED_MARKER} {chars} characters for {path}, sent in an earlier request. \
             Read the file for its current contents.]"
        );
        omitted = true;
    }
    if !omitted {
        return None;
    }
    RawValue::from_string(Value::Object(args).to_string()).ok()
}

/// A file write whose text is a copied note would overwrite real content with it.
pub(crate) fn copies_an_omission_note(call: &ToolCall) -> bool {
    if !MUTATING_TOOLS.contains(&call.name.as_str()) {
        return false;
    }
    let Ok(Value::Object(args)) = serde_json::from_str(call.args.get()) else {
        return false;
    };
    args.values().filter_map(Value::as_str).any(|text| {
        let text = text.trim();
        text.starts_with(OMITTED_MARKER) && text.chars().count() <= MAX_NOTE_CHARS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            args: RawValue::from_string(args.to_string()).unwrap(),
        }
    }

    fn asking(call: ToolCall) -> Message {
        let mut m = Message::assistant("");
        m.tool_calls.push(call);
        m
    }

    fn args_of(m: &Message) -> Value {
        serde_json::from_str(m.tool_calls[0].args.get()).unwrap()
    }

    fn big(n: usize) -> String {
        "x".repeat(n)
    }

    /// A finished request that wrote a file, then a new one starting at index 5.
    fn two_requests(written: &str) -> (Vec<Message>, usize) {
        let messages = vec![
            Message::system("sys"),
            Message::user("make a page"),
            asking(call(
                "write_file",
                serde_json::json!({"path": "index.html", "content": written}),
            )),
            Message::tool_result("call-write_file", "write_file", "wrote index.html"),
            Message::assistant("done"),
            Message::user("what did you build?"),
        ];
        (messages, 5)
    }

    #[test]
    fn a_big_file_written_in_an_earlier_request_is_left_out_of_what_is_sent() {
        let (messages, start) = two_requests(&big(5_000));
        let sent = lighter_history(&messages, start).expect("something to omit");
        let args = args_of(&sent[2]);
        assert_eq!(args["path"], "index.html");
        let note = args["content"].as_str().unwrap();
        assert!(note.starts_with(OMITTED_MARKER), "{note}");
        assert!(note.contains("5000 characters for index.html"), "{note}");
        assert!(note.contains("Read the file"), "{note}");
        assert_eq!(sent.len(), messages.len());
        for (i, (a, b)) in sent.iter().zip(&messages).enumerate() {
            if i != 2 {
                assert_eq!(
                    serde_json::to_string(a).unwrap(),
                    serde_json::to_string(b).unwrap()
                );
            }
        }
        assert_eq!(sent[2].tool_calls[0].id, messages[2].tool_calls[0].id);
    }

    #[test]
    fn the_stored_transcript_is_never_changed() {
        let (messages, start) = two_requests(&big(5_000));
        let before = serde_json::to_string(&messages).unwrap();
        let _ = lighter_history(&messages, start);
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
    }

    #[test]
    fn the_request_in_flight_is_sent_whole() {
        let (messages, _) = two_requests(&big(5_000));
        assert!(lighter_history(&messages, 1).is_none());
        assert!(lighter_history(&messages, 0).is_none());
    }

    #[test]
    fn nothing_changes_when_there_is_nothing_worth_omitting() {
        let (small, start) = two_requests("<h1>hi</h1>");
        assert!(lighter_history(&small, start).is_none());
        let (edge, start) = two_requests(&big(OMIT_ABOVE_CHARS));
        assert!(lighter_history(&edge, start).is_none());
        let (over, start) = two_requests(&big(OMIT_ABOVE_CHARS + 1));
        assert!(lighter_history(&over, start).is_some());
    }

    #[test]
    fn only_file_writes_are_touched() {
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            asking(call(
                "run_shell",
                serde_json::json!({"command": big(5_000)}),
            )),
            asking(call(
                "create_pdf",
                serde_json::json!({"path": "a.pdf", "markdown": big(5_000)}),
            )),
            Message::tool_result("x", "read_file", big(5_000)),
            Message::user("next"),
        ];
        assert!(lighter_history(&messages, 5).is_none());
    }

    #[test]
    fn an_edit_keeps_its_short_fields_and_loses_its_long_ones() {
        let messages = vec![
            Message::system("sys"),
            Message::user("fix it"),
            asking(call(
                "edit_file",
                serde_json::json!({
                    "path": "app.js",
                    "old_string": "let a = 1;",
                    "new_string": big(3_000),
                    "replace_all": false,
                }),
            )),
            Message::user("thanks"),
        ];
        let sent = lighter_history(&messages, 3).unwrap();
        let args = args_of(&sent[2]);
        assert_eq!(args["path"], "app.js");
        assert_eq!(args["old_string"], "let a = 1;");
        assert_eq!(args["replace_all"], false);
        assert!(
            args["new_string"]
                .as_str()
                .unwrap()
                .starts_with(OMITTED_MARKER)
        );
    }

    #[test]
    fn the_same_history_is_sent_the_same_way_every_turn() {
        // Byte-identical, so the provider's prompt cache keeps hitting.
        let (messages, start) = two_requests(&big(5_000));
        let first = serde_json::to_string(&lighter_history(&messages, start)).unwrap();
        let second = serde_json::to_string(&lighter_history(&messages, start)).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn arguments_that_are_not_an_object_are_left_alone() {
        let messages = vec![
            Message::system("s"),
            asking(call("write_file", serde_json::json!([big(5_000)]))),
            Message::user("u"),
        ];
        assert!(lighter_history(&messages, 2).is_none());
    }

    #[test]
    fn a_write_whose_text_is_a_copied_note_is_caught() {
        let (messages, start) = two_requests(&big(5_000));
        let sent = lighter_history(&messages, start).unwrap();
        assert!(copies_an_omission_note(&sent[2].tool_calls[0]));
        let imitation = call(
            "edit_file",
            serde_json::json!({"path": "a.js", "old_string": "x", "new_string": format!("  {OMITTED_MARKER} 10 characters]  ")}),
        );
        assert!(copies_an_omission_note(&imitation));
    }

    #[test]
    fn real_content_that_mentions_the_marker_is_not_mistaken_for_a_note() {
        let source = format!(
            "const OMITTED_MARKER: &str = \"{OMITTED_MARKER}\";\n{}",
            big(500)
        );
        assert!(!copies_an_omission_note(&call(
            "write_file",
            serde_json::json!({"path": "lean.rs", "content": source}),
        )));
        assert!(!copies_an_omission_note(&call(
            "write_file",
            serde_json::json!({"path": "a.md", "content": "hello"}),
        )));
        assert!(!copies_an_omission_note(&call(
            "run_shell",
            serde_json::json!({"command": format!("{OMITTED_MARKER}]")}),
        )));
    }
}
