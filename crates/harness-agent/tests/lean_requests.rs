//! The real agent loop against a scripted provider: what it sends, and what it keeps.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_agent::{Agent, Ui};
use harness_config::{AgentPolicy, Backend, Dialect, Endpoint, Resolved};
use harness_tools::{Registry, Workspace, WriteFile};
use harness_types::{Role, Usage};
use serde_json::json;

struct Quiet;

impl Ui for Quiet {
    fn turn_started(&self) {}
    fn assistant_delta(&self, _: &str) {}
    fn reasoning_delta(&self, _: &str) {}
    fn assistant_done(&self) {}
    fn tool_start(&self, _: &str, _: &str) {}
    fn tool_end(&self, _: &str, _: &str, _: bool, _: f64, _: f64) {}
    fn usage(&self, _: &Usage, _: &str, _: Backend, _: f64) {}
    fn retrying(&self, _: u32, _: u32, _: Duration, _: &str) {}
    fn model_escalated(&self, _: &str, _: &str, _: &str) {}
    fn interjected(&self, _: usize) {}
    fn compacted(&self, _: usize, _: usize, _: u64, _: Option<f64>) {}
    fn stopped_for_budget(&self, _: f64, _: f64) {}
    fn stopped_for_context_limit(&self, _: u64, _: u64) {}
    fn tool_progress(&self, _: &str, _: &str) {}
}

fn sse(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn writes(path: &str, content: &str) -> String {
    let args = json!({"path": path, "content": content}).to_string();
    sse(&[
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"w1","type":"function","function":{"name":"write_file","arguments":args}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

fn says(text: &str) -> String {
    sse(&[
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

/// Answers each request with the next scripted reply and records every body.
fn provider(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let mut sock = stream.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let body_start = loop {
                let n = sock.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&raw[..body_start]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            while raw.len() < body_start + length {
                let n = sock.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw[body_start..]).into_owned());
            let reply = replies.next().expect("an unscripted request");
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{reply}"
                )
                .as_bytes(),
            );
        }
    });
    (url, bodies)
}

fn workspace(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hm-lean-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn agent(url: String, root: &std::path::Path, model: &str) -> Agent {
    let ws = Workspace::new(root);
    let mut tools = Registry::new();
    tools.register(Arc::new(WriteFile(ws.clone())));
    let resolved = Resolved {
        endpoint: Endpoint {
            base_url: url,
            api_key: "k".into(),
        },
        default_model: model.into(),
        backend: Backend::Byok,
        dialect: Dialect::OpenAiCompatible,
        reasoning_effort: None,
        budget_usd: None,
        policy: AgentPolicy::default(),
        hooks: Vec::new(),
    };
    Agent::new(
        resolved,
        tools,
        ws,
        Arc::new(Quiet),
        "you are a test".into(),
    )
}

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
