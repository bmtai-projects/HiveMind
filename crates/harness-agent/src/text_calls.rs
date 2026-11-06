//! Tool calls a local model wrote out as text instead of making them.

use serde_json::Value;
use serde_json::value::RawValue;

const FENCE: &str = "```";
const OPEN_TAG: &str = "<tool_call>";
const CLOSE_TAG: &str = "</tool_call>";

/// Whether text streamed so far could still turn out to be a written call, so it is held back.
pub(crate) fn may_be_a_call(text: &str) -> bool {
    let t = text.trim_start();
    if OPEN_TAG.starts_with(t) {
        return true;
    }
    match t.strip_prefix(OPEN_TAG) {
        Some(rest) => may_be_a_body(rest.trim_start()),
        None => may_be_a_body(t),
    }
}

fn may_be_a_body(t: &str) -> bool {
    if FENCE.starts_with(t) {
        return true;
    }
    let Some(after_fence) = t.strip_prefix(FENCE) else {
        return may_be_json_call(t);
    };
    match after_fence.split_once('\n') {
        None => "json".starts_with(&after_fence.trim().to_ascii_lowercase()),
        Some((language, body)) => {
            is_json_fence(language) && {
                let body = body.trim_start();
                body.is_empty() || may_be_json_call(body)
            }
        }
    }
}

/// `{"…` or `[{"…`, as much of it as has arrived.
fn may_be_json_call(t: &str) -> bool {
    if let Some(rest) = t.strip_prefix('[') {
        let rest = rest.trim_start();
        return rest.is_empty() || may_be_json_call(rest);
    }
    t.strip_prefix('{').is_some_and(|rest| {
        let rest = rest.trim_start();
        rest.is_empty() || rest.starts_with('"')
    })
}

fn is_json_fence(language: &str) -> bool {
    let language = language.trim();
    language.is_empty() || language.eq_ignore_ascii_case("json")
}

/// The `(name, arguments)` calls `text` consists of, when all of it is calls to known tools.
pub(crate) fn parse(
    text: &str,
    known: impl Fn(&str) -> bool,
) -> Option<Vec<(String, Box<RawValue>)>> {
    let text = text.trim();
    let bodies = if text.starts_with(OPEN_TAG) {
        tagged_bodies(text)?
    } else {
        vec![unfenced(text)?]
    };
    let mut calls = Vec::new();
    for body in bodies {
        for value in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
            match value.ok()? {
                Value::Array(items) => {
                    for item in items {
                        calls.push(one_call(item, &known)?);
                    }
                }
                item => calls.push(one_call(item, &known)?),
            }
        }
    }
    (!calls.is_empty()).then_some(calls)
}

fn tagged_bodies(text: &str) -> Option<Vec<&str>> {
    let mut bodies = Vec::new();
    let mut rest = text;
    while let Some(after_open) = rest.trim_start().strip_prefix(OPEN_TAG) {
        let (body, after) = after_open.split_once(CLOSE_TAG).unwrap_or((after_open, ""));
        bodies.push(unfenced(body.trim())?);
        rest = after;
    }
    rest.trim().is_empty().then_some(bodies)
}

/// `text` without a surrounding Markdown code fence, if it has one.
fn unfenced(text: &str) -> Option<&str> {
    let Some(inner) = text.strip_prefix(FENCE) else {
        return Some(text);
    };
    let (language, body) = inner.split_once('\n')?;
    if !is_json_fence(language) {
        return None;
    }
    body.trim_end().strip_suffix(FENCE).map(str::trim)
}

fn one_call(item: Value, known: &impl Fn(&str) -> bool) -> Option<(String, Box<RawValue>)> {
    let Value::Object(mut call) = item else {
        return None;
    };
    // The OpenAI shape nests both fields under "function".
    if let Some(Value::Object(function)) = call.remove("function") {
        call = function;
    }
    let name = call.get("name")?.as_str()?.to_string();
    if !known(&name) {
        return None;
    }
    let args = match call
        .remove("arguments")
        .or_else(|| call.remove("parameters"))
    {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(Value::String(encoded)) => serde_json::from_str(&encoded).ok()?,
        Some(args) => args,
    };
    if !args.is_object() {
        return None;
    }
    Some((name, RawValue::from_string(args.to_string()).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(name: &str) -> bool {
        matches!(
            name,
            "todo_write" | "project_map" | "read_file" | "write_file"
        )
    }

    fn calls(text: &str) -> Option<Vec<(String, String)>> {
        parse(text, known).map(|c| {
            c.into_iter()
                .map(|(name, args)| (name, args.get().to_string()))
                .collect()
        })
    }

    #[test]
    fn the_reply_from_the_screenshot_is_a_call() {
        let reply = r#"{
  "name": "todo_write",
  "arguments": {
    "todos": [
      {"content": "Understand the project structure", "status": "completed"}
    ]
  }
}"#;
        let found = calls(reply).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "todo_write");
        assert!(found[0].1.contains("Understand the project structure"));
    }

    #[test]
    fn the_usual_ways_models_write_a_call_are_all_understood() {
        for text in [
            r#"{"name" : "project_map", "arguments" : {}}"#,
            r#"{"name": "project_map"}"#,
            r#"{"name": "read_file", "parameters": {"path": "a.rs"}}"#,
            r#"{"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}"#,
            r#"{"type": "function", "function": {"name": "read_file", "arguments": {"path": "a.rs"}}}"#,
            "```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n```",
            "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>",
            "  \n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n  ",
        ] {
            let found = calls(text).unwrap_or_else(|| panic!("not understood: {text}"));
            assert_eq!(found.len(), 1, "{text}");
        }
    }

    #[test]
    fn several_calls_in_one_reply_are_all_kept_in_order() {
        let tagged = "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}</tool_call>\n<tool_call>{\"name\":\"project_map\",\"arguments\":{}}</tool_call>";
        let listed = r#"[{"name":"read_file","arguments":{"path":"a"}},{"name":"project_map","arguments":{}}]"#;
        let stacked = "{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}\n{\"name\":\"project_map\",\"arguments\":{}}";
        for text in [tagged, listed, stacked] {
            let names: Vec<String> = calls(text).unwrap().into_iter().map(|c| c.0).collect();
            assert_eq!(names, ["read_file", "project_map"], "{text}");
        }
    }

    #[test]
    fn ordinary_replies_are_left_alone() {
        for text in [
            "Hello! How can I assist you today?",
            "",
            "Here is the call: {\"name\": \"read_file\", \"arguments\": {}}",
            "{\"name\": \"read_file\", \"arguments\": {}} and then I will edit it",
            "```json\n{\"name\": \"read_file\"}\n``` that is the call",
            "{\"name\": \"my-app\", \"version\": \"1.0.0\"}",
            "{\"name\": \"read_text\", \"arguments\": {\"path\": \"x\"}}",
            "{\"name\": \"read_file\", \"arguments\": [1, 2]}",
            "{\"name\": \"read_file\", \"arguments\": {}",
            "[]",
            "<tool_call>{\"name\":\"read_file\"}</tool_call> trailing words",
        ] {
            assert!(calls(text).is_none(), "should stay text: {text}");
        }
    }

    #[test]
    fn one_unknown_tool_keeps_the_whole_reply_as_text() {
        let text =
            r#"[{"name":"read_file","arguments":{}},{"name":"delete_everything","arguments":{}}]"#;
        assert!(calls(text).is_none());
    }

    #[test]
    fn text_is_held_back_only_while_it_could_still_be_a_call() {
        for held in [
            "",
            "  ",
            "{",
            "\n  {\"na",
            "[",
            "`",
            "``",
            "```json",
            "<tool",
            "<tool_call>{",
            "<tool_call>\n```json\n{",
            "[{\"name",
            "```j",
            "```JSON\n",
            "```\n  {\"name",
        ] {
            assert!(may_be_a_call(held), "{held:?}");
        }
        for shown in [
            "Hello",
            "Sure, ",
            "I'll",
            "<div>",
            "``x",
            "- item",
            "```python",
            "```rust\nfn main() {",
            "```json\nconst x",
            "[1] first point",
            "[Link](https://example.com)",
            "{x}",
            "<tool_call>hello",
        ] {
            assert!(!may_be_a_call(shown), "{shown:?}");
        }
    }

    #[test]
    fn a_call_fenced_as_another_language_is_not_a_call() {
        assert!(calls("```python\n{\"name\": \"read_file\"}\n```").is_none());
        assert!(calls("```JSON\n{\"name\": \"read_file\"}\n```").is_some());
    }
}
