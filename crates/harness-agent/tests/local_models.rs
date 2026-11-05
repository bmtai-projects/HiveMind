//! Local models: tool calls written as text, and prompts the local server cut short.

mod common;

use common::{agent_with, provider, says, says_in, workspace};
use harness_config::Backend;
use harness_types::Role;

const WRITTEN_CALL: &str = "{\n  \"name\": \"write_file\",\n  \"arguments\": {\"path\": \"a.txt\", \"content\": \"hi\"}\n}";

#[tokio::test]
async fn a_tool_call_written_as_text_runs_and_is_never_shown() {
    let root = workspace("written-call");
    // Split mid-name, the way a stream delivers it.
    let (first, rest) = WRITTEN_CALL.split_at(WRITTEN_CALL.find("_file").unwrap());
    let (url, bodies) = provider(vec![says_in(&[first, rest], 10), says("done")]);
    let (mut agent, ui) = agent_with(url, &root, "qwen2.5-coder:7b", Backend::Local, "test");

    agent.run("make a.txt").await.unwrap();

    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hi");
    assert_eq!(
        *ui.shown.lock().unwrap(),
        "done",
        "the JSON never reached the screen"
    );
    let asked = agent
        .history()
        .iter()
        .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("stored as a real tool call");
    assert_eq!(asked.tool_calls[0].name, "write_file");
    assert!(asked.content.is_empty());
    let next: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[1]).unwrap();
    let roles: Vec<&str> = next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn ordinary_replies_from_a_local_model_reach_the_screen_unchanged() {
    let root = workspace("local-text");
    let json_answer = "{\"name\": \"my-app\", \"version\": \"1.0.0\"}";
    let (url, _) = provider(vec![
        says_in(&["Hel", "lo ", "there"], 10),
        says_in(&["{\"name\": ", "\"my-app\", \"version\": \"1.0.0\"}"], 10),
    ]);
    let (mut agent, ui) = agent_with(url, &root, "qwen2.5-coder:7b", Backend::Local, "test");

    agent.run("hi").await.unwrap();
    assert_eq!(*ui.shown.lock().unwrap(), "Hello there");
    agent.run("show package.json's name").await.unwrap();
    assert_eq!(
        *ui.shown.lock().unwrap(),
        format!("Hello there{json_answer}")
    );
    assert!(
        agent.history().iter().all(|m| m.tool_calls.is_empty()),
        "JSON that names no tool stays a reply"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_code_answer_from_a_local_model_still_streams_as_it_arrives() {
    let root = workspace("local-code");
    let (url, _) = provider(vec![says_in(
        &[
            "```py",
            "thon
print(1)",
            "
```",
        ],
        10,
    )]);
    let (mut agent, ui) = agent_with(url, &root, "qwen2.5-coder:7b", Backend::Local, "test");

    agent.run("print one").await.unwrap();

    assert_eq!(
        *ui.shown.lock().unwrap(),
        "```python
print(1)
```"
    );
    assert_eq!(
        *ui.pieces.lock().unwrap(),
        3,
        "shown piece by piece, not held"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_provider_that_is_not_local_never_has_text_turned_into_calls() {
    let root = workspace("byok-text");
    let (url, bodies) = provider(vec![says(WRITTEN_CALL)]);
    let (mut agent, ui) = agent_with(url, &root, "some/model", Backend::Byok, "test");

    agent.run("make a.txt").await.unwrap();

    assert_eq!(bodies.lock().unwrap().len(), 1);
    assert!(!root.join("a.txt").exists());
    assert_eq!(*ui.shown.lock().unwrap(), WRITTEN_CALL);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_prompt_the_local_server_cut_short_is_reported_once_per_request() {
    let root = workspace("cut");
    let long_prompt = "rule ".repeat(6_000);
    let (url, _) = provider(vec![
        says_in(&["hi"], 2_050),
        says_in(&["hi again"], 2_050),
        says_in(&["all read"], 9_000),
    ]);
    let (mut agent, ui) = agent_with(url, &root, "qwen2.5-coder:7b", Backend::Local, &long_prompt);

    agent.run("hi").await.unwrap();
    let notices = ui.notices.lock().unwrap().clone();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("read only 2050 tokens"),
        "{}",
        notices[0]
    );
    assert!(
        notices[0].contains("OLLAMA_CONTEXT_LENGTH"),
        "{}",
        notices[0]
    );

    agent.run("again").await.unwrap();
    assert_eq!(
        ui.notices.lock().unwrap().len(),
        2,
        "still cut, so said again"
    );

    agent.run("and now?").await.unwrap();
    assert_eq!(
        ui.notices.lock().unwrap().len(),
        2,
        "read in full: nothing to say"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn short_requests_and_other_providers_get_no_cut_warning() {
    let root = workspace("no-cut");
    let (url, _) = provider(vec![says_in(&["hi"], 10)]);
    let (mut agent, ui) = agent_with(url, &root, "qwen2.5-coder:7b", Backend::Local, "test");
    agent.run("hi").await.unwrap();
    assert!(ui.notices.lock().unwrap().is_empty());

    let (url, _) = provider(vec![says_in(&["hi"], 10)]);
    let long_prompt = "rule ".repeat(6_000);
    let (mut agent, ui) = agent_with(url, &root, "some/model", Backend::Byok, &long_prompt);
    agent.run("hi").await.unwrap();
    assert!(ui.notices.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}
