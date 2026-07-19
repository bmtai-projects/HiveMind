//! The unified `Tool` trait and its registry — analogue of grok-build's
//! `xai-tool-runtime` `Tool` trait + `ToolBridge`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::value::RawValue;

use harness_types::{ToolCall, ToolSchema};

use crate::error::ToolError;

/// A single capability the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema object describing the arguments.
    fn schema(&self) -> serde_json::Value;
    /// Run the tool. The returned string is fed back to the model as the
    /// tool result. An `Err` is surfaced to the model as an error result —
    /// the agent loop keeps going; tool failure is never fatal.
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError>;
}

/// Registered tools, keyed by name in a `BTreeMap` so [`Registry::schemas`]
/// is name-sorted — a deliberately stable order, since the tool manifest
/// sits in the prompt prefix DeepSeek's context cache keys on. Reordering
/// tools between turns would silently break the cache hit rate.
#[derive(Clone, Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.schema(),
            })
            .collect()
    }

    /// Execute every call in `calls` concurrently, then return results in
    /// the **same order the calls arrived in** — concurrency changes
    /// completion timing, not the message sequence the model sees next.
    /// Stable ordering matters twice over: it keeps the transcript
    /// deterministic for the user, and it keeps the resulting prefix
    /// consistent turn-to-turn for prompt caching.
    pub async fn dispatch_many(&self, calls: Vec<ToolCall>) -> Vec<(ToolCall, String)> {
        let mut set = tokio::task::JoinSet::new();
        for (idx, call) in calls.into_iter().enumerate() {
            let tool = self.tools.get(&call.name).cloned();
            set.spawn(async move {
                let result = match tool {
                    Some(t) => match t.execute(&call.args).await {
                        Ok(s) => s,
                        Err(e) => format!("ERROR: {e}"),
                    },
                    None => format!("ERROR: unknown tool \"{}\"", call.name),
                };
                (idx, call, result)
            });
        }

        let mut slots: Vec<Option<(ToolCall, String)>> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((idx, call, result)) => {
                    if slots.len() <= idx {
                        slots.resize_with(idx + 1, || None);
                    }
                    slots[idx] = Some((call, result));
                }
                Err(join_err) => {
                    // A tool panicked. Surface it as a synthetic failure
                    // rather than losing the slot (and the model's turn
                    // would otherwise hang waiting for a result it never gets).
                    tracing_stub(&format!("tool task panicked: {join_err}"));
                }
            }
        }
        slots.into_iter().flatten().collect()
    }
}

/// No logging framework dependency in this crate; stderr is enough for a
/// panic that should never happen in practice.
fn tracing_stub(msg: &str) {
    eprintln!("[harness-tools] {msg}");
}

/// Small JSON-Schema object builder so tool definitions stay readable.
pub fn obj_schema(props: &[(&str, serde_json::Value)], required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}
