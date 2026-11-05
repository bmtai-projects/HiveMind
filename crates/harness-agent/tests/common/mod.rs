//! A scripted OpenAI-compatible provider and a recording UI, shared by the agent loop tests.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_agent::{Agent, Ui};
use harness_config::{AgentPolicy, Backend, Dialect, Endpoint, Resolved};
use harness_tools::{Registry, Workspace, WriteFile};
use harness_types::Usage;
use serde_json::json;

/// Keeps what reached the user: streamed reply text and notices.
#[derive(Default)]
pub struct Recorder {
    pub shown: Mutex<String>,
    /// How many separate pieces the reply text arrived in.
    pub pieces: Mutex<usize>,
    pub notices: Mutex<Vec<String>>,
}

impl Ui for Recorder {
    fn diagnostic(&self, message: &str) {
        self.notices.lock().unwrap().push(message.to_string());
    }
    fn turn_started(&self) {}
    fn assistant_delta(&self, text: &str) {
        self.shown.lock().unwrap().push_str(text);
        *self.pieces.lock().unwrap() += 1;
    }
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

pub fn sse(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

pub fn writes(path: &str, content: &str) -> String {
    let args = json!({"path": path, "content": content}).to_string();
    sse(&[
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"w1","type":"function","function":{"name":"write_file","arguments":args}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

pub fn says(text: &str) -> String {
    says_in(&[text], 10)
}

/// A text reply streamed in `pieces`, reporting `prompt_tokens` as read.
pub fn says_in(pieces: &[&str], prompt_tokens: u64) -> String {
    let mut chunks: Vec<serde_json::Value> = pieces
        .iter()
        .map(|p| json!({"choices":[{"index":0,"delta":{"content":p}}]}))
        .collect();
    chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":prompt_tokens,"completion_tokens":5,"total_tokens":prompt_tokens + 5}}));
    sse(&chunks)
}

/// Answers each request with the next scripted reply and records every body.
pub fn provider(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
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

pub fn workspace(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hm-agent-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An agent with `write_file` only, talking to `url`.
pub fn agent_with(
    url: String,
    root: &std::path::Path,
    model: &str,
    backend: Backend,
    system_prompt: &str,
) -> (Agent, Arc<Recorder>) {
    let ws = Workspace::new(root);
    let mut tools = Registry::new();
    tools.register(Arc::new(WriteFile(ws.clone())));
    let resolved = Resolved {
        endpoint: Endpoint {
            base_url: url,
            api_key: if backend == Backend::Local { "" } else { "k" }.into(),
        },
        default_model: model.into(),
        backend,
        dialect: Dialect::OpenAiCompatible,
        reasoning_effort: None,
        budget_usd: None,
        policy: AgentPolicy::default(),
        hooks: Vec::new(),
    };
    let ui = Arc::new(Recorder::default());
    let agent = Agent::new(resolved, tools, ws, ui.clone(), system_prompt.into());
    (agent, ui)
}

pub fn agent(url: String, root: &std::path::Path, model: &str) -> Agent {
    agent_with(url, root, model, Backend::Byok, "you are a test").0
}
