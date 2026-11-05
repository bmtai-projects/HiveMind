//! The real agent loop against a scripted provider: what it sends, and what it keeps.

mod common;

use common::{agent, provider, says, workspace, writes};
use harness_types::Role;

fn sent_page(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let call = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|m| m["tool_calls"].as_array().map(|c| c[0].clone()))
        .expect("the write_file call is in the history");
    let args: serde_json::Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    args["content"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_finished_write_is_sent_whole_in_its_request_and_as_a_note_after_it() {
    let page = format!("<html>{}</html>", "x".repeat(6_000));
    let root = workspace("loose");
    let (url, bodies) = provider(vec![
        writes("page.html", &page),
        says("done"),
        says("a page"),
    ]);
    let mut agent = agent(url, &root, "test-model");

    agent.run("make a page").await.unwrap();
    agent.run("what did you build?").await.unwrap();

    let bodies = bodies.lock().unwrap();
    assert_eq!(
        bodies.len(),
        3,
        "a loose page gets no extra turn asking for a check"
    );
    assert_eq!(sent_page(&bodies[1]), page, "same request: sent whole");
    let note = sent_page(&bodies[2]);
    assert!(note.starts_with("[omitted to save tokens:"), "{note}");
    assert!(note.contains("page.html"), "{note}");
    assert!(!bodies[2].contains(&"x".repeat(6_000)));
    for body in bodies.iter() {
        assert!(!body.contains("cache_control"), "not Claude: {body}");
    }

    // Kept whole on disk and in the stored history.
    assert_eq!(
        std::fs::read_to_string(root.join("page.html")).unwrap(),
        page
    );
    let stored = agent
        .history()
        .iter()
        .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .unwrap();
    assert!(stored.tool_calls[0].args.get().contains(&"x".repeat(6_000)));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_model_that_copies_the_note_back_cannot_overwrite_the_file() {
    let page = format!("<html>{}</html>", "x".repeat(6_000));
    let note = "[omitted to save tokens: 6013 characters for page.html, sent in an earlier request. \
                Read the file for its current contents.]";
    let root = workspace("copied");
    let (url, bodies) = provider(vec![
        writes("page.html", &page),
        says("done"),
        writes("page.html", note),
        says("sorry, reading it first"),
    ]);
    let mut agent = agent(url, &root, "test-model");

    agent.run("make a page").await.unwrap();
    agent.run("make the title red").await.unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("page.html")).unwrap(),
        page
    );
    let bodies = bodies.lock().unwrap();
    assert!(bodies[3].contains("Nothing was written"), "{}", bodies[3]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_page_inside_a_project_still_gets_the_check_note() {
    let root = workspace("project");
    std::fs::write(root.join("package.json"), "{}").unwrap();
    let (url, bodies) = provider(vec![
        writes("index.html", "<h1>hi</h1>"),
        says("done"),
        says("nothing to run"),
    ]);
    let mut agent = agent(url, &root, "test-model");

    agent.run("make a page").await.unwrap();

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3, "the note bought one more turn, as before");
    assert!(bodies[2].contains("<harness-note>"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_short_conversation_goes_out_exactly_as_stored() {
    let root = workspace("short");
    let (url, bodies) = provider(vec![says("hi"), says("still here")]);
    let mut agent = agent(url, &root, "test-model");

    agent.run("hello").await.unwrap();
    agent.run("again").await.unwrap();

    let bodies = bodies.lock().unwrap();
    let sent: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(sent["messages"][2]["content"], "hi");
    assert!(sent.get("cache_control").is_none());
    let _ = std::fs::remove_dir_all(&root);
}
