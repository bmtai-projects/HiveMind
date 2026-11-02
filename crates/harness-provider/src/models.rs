//! Asking a provider which models it offers, and what it says about each.
//!
//! OpenAI-style servers answer `GET {base}/models` with `{"data": [{"id": ..}]}`.
//! OpenRouter adds a context size, per-token prices, release date, the
//! parameters each model supports, benchmark scores and, for reasoning models,
//! the exact effort levels each accepts. Anthropic answers with `display_name`.
//! Everything beyond `id` is optional, and a missing field means "unknown".

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use serde_json::Value;

use crate::error::ProviderError;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    /// Reasoning levels the provider says this model accepts. `None` when it
    /// does not say, which is different from `Some(vec![])`.
    pub reasoning_efforts: Option<Vec<String>>,
    pub input_per_m: Option<f64>,
    pub output_per_m: Option<f64>,
    /// Release time, in seconds since the Unix epoch.
    pub created: Option<i64>,
    /// Whether the model supports tool calling. `None` when the provider
    /// doesn't list supported parameters.
    pub supports_tools: Option<bool>,
    /// Whether the model produces text. `None` when the provider doesn't say.
    pub text_output: Option<bool>,
    /// Artificial Analysis Intelligence Index (0-100), as published by the
    /// provider. A third-party benchmark, not HiveMind's own measurement.
    pub intelligence: Option<f64>,
}

pub type ModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<RemoteModel>, ProviderError>> + Send + 'a>>;

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<RawModel>,
}

/// Loosely typed on purpose: one odd value in one model's entry must not make
/// the whole list unreadable.
#[derive(Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_length: Option<Value>,
    #[serde(default)]
    created: Option<Value>,
    #[serde(default)]
    pricing: Option<RawPricing>,
    #[serde(default)]
    reasoning: Option<RawReasoning>,
    #[serde(default)]
    supported_parameters: Option<Value>,
    #[serde(default)]
    architecture: Option<Value>,
    #[serde(default)]
    benchmarks: Option<Value>,
}

#[derive(Deserialize)]
struct RawPricing {
    #[serde(default)]
    prompt: Option<Price>,
    #[serde(default)]
    completion: Option<Price>,
}

/// Prices arrive as strings ("0.00000014") or numbers, in dollars per token.
#[derive(Deserialize)]
#[serde(untagged)]
enum Price {
    Number(f64),
    Text(String),
}

impl Price {
    /// Dollars per million tokens. Negative prices mean "varies" (routers).
    fn per_million(&self) -> Option<f64> {
        let per_token = match self {
            Self::Number(n) => *n,
            Self::Text(s) => s.trim().parse().ok()?,
        };
        (per_token >= 0.0).then_some(per_token * 1_000_000.0)
    }
}

#[derive(Deserialize)]
struct RawReasoning {
    #[serde(default)]
    supported_efforts: Option<Vec<String>>,
}

fn strings(value: &Value) -> Option<Vec<&str>> {
    Some(value.as_array()?.iter().filter_map(Value::as_str).collect())
}

pub(crate) fn parse_models(json: &str) -> Result<Vec<RemoteModel>, ProviderError> {
    let list: ModelList = serde_json::from_str(json)
        .map_err(|e| ProviderError::Request(format!("the model list was not understood: {e}")))?;
    Ok(list
        .data
        .into_iter()
        .filter(|m| !m.id.trim().is_empty())
        .map(|m| RemoteModel {
            supports_tools: m
                .supported_parameters
                .as_ref()
                .and_then(strings)
                .map(|params| params.contains(&"tools")),
            text_output: m
                .architecture
                .as_ref()
                .and_then(|a| strings(a.get("output_modalities")?))
                .map(|modalities| modalities.contains(&"text")),
            intelligence: m.benchmarks.as_ref().and_then(|b| {
                b.get("artificial_analysis")?
                    .get("intelligence_index")?
                    .as_f64()
            }),
            created: m
                .created
                .as_ref()
                .and_then(|c| c.as_i64().or_else(|| c.as_f64().map(|f| f as i64))),
            context_window: m
                .context_length
                .as_ref()
                .and_then(Value::as_u64)
                .filter(|n| *n > 0),
            id: m.id,
            name: m.name.or(m.display_name).filter(|n| !n.trim().is_empty()),
            reasoning_efforts: m.reasoning.and_then(|r| r.supported_efforts),
            input_per_m: m
                .pricing
                .as_ref()
                .and_then(|p| p.prompt.as_ref()?.per_million()),
            output_per_m: m
                .pricing
                .as_ref()
                .and_then(|p| p.completion.as_ref()?.per_million()),
        })
        .collect())
}

/// Sends a prepared request for a model list and decodes the answer.
pub(crate) async fn fetch_models(
    req: reqwest::RequestBuilder,
) -> Result<Vec<RemoteModel>, ProviderError> {
    let resp = req
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ProviderError::Http {
            status: status.as_u16(),
            body: text.chars().take(300).collect(),
            retry_after_secs: None,
        });
    }
    parse_models(&text)
}

/// Only OpenRouter answers a `category` filter, so only it is asked.
pub(crate) fn is_openrouter(base_url: &str) -> bool {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openai_style_list_gives_ids_and_nothing_else() {
        let models = parse_models(
            r#"{"object":"list","data":[{"id":"llama3","object":"model"},{"id":"qwen2.5"}]}"#,
        )
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3");
        assert_eq!(models[0].name, None);
        assert_eq!(
            models[0].reasoning_efforts, None,
            "silence is unknown, not empty"
        );
        assert_eq!(models[0].input_per_m, None);
        assert_eq!(models[0].supports_tools, None);
        assert_eq!(models[0].text_output, None);
        assert_eq!(models[0].intelligence, None);
        assert_eq!(models[0].created, None);
    }

    #[test]
    fn openrouter_fields_are_read_including_the_exact_reasoning_levels() {
        let json = r#"{"data":[
            {"id":"deepseek/deepseek-v4-flash","name":"DeepSeek: V4 Flash","context_length":1048576,
             "pricing":{"prompt":"0.00000014","completion":"0.00000028"},
             "reasoning":{"mandatory":false,"supported_efforts":["xhigh","high","none"],"default_effort":"high"}},
            {"id":"openrouter/auto","name":"Auto","context_length":2000000,
             "pricing":{"prompt":"-1","completion":"-1"}}
        ]}"#;
        let models = parse_models(json).unwrap();
        let flash = &models[0];
        assert_eq!(flash.name.as_deref(), Some("DeepSeek: V4 Flash"));
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(
            flash.reasoning_efforts.as_deref(),
            Some(&["xhigh".to_string(), "high".to_string(), "none".to_string()][..])
        );
        assert!((flash.input_per_m.unwrap() - 0.14).abs() < 1e-9);
        assert!((flash.output_per_m.unwrap() - 0.28).abs() < 1e-9);
        assert_eq!(
            models[1].input_per_m, None,
            "a negative price means it varies"
        );
        assert_eq!(models[1].reasoning_efforts, None);
    }

    #[test]
    fn tool_support_text_output_release_date_and_benchmarks_are_read() {
        let json = r#"{"data":[
            {"id":"a/smart","created":1790000000,
             "supported_parameters":["max_tokens","tools","reasoning"],
             "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},
             "benchmarks":{"design_arena":[],"artificial_analysis":{"intelligence_index":57.6,"coding_index":null}}},
            {"id":"a/imagegen","supported_parameters":["max_tokens"],
             "architecture":{"output_modalities":["image"]},
             "benchmarks":{"artificial_analysis":{"intelligence_index":null}}},
            {"id":"a/odd","created":"soon","supported_parameters":"tools",
             "architecture":[1],"benchmarks":{"artificial_analysis":"n/a"},"context_length":"big"}
        ]}"#;
        let models = parse_models(json).unwrap();
        let smart = &models[0];
        assert_eq!(smart.supports_tools, Some(true));
        assert_eq!(smart.text_output, Some(true));
        assert_eq!(smart.created, Some(1_790_000_000));
        assert_eq!(smart.intelligence, Some(57.6));
        let image = &models[1];
        assert_eq!(image.supports_tools, Some(false));
        assert_eq!(image.text_output, Some(false));
        assert_eq!(image.intelligence, None, "a null score is no score");
        let odd = &models[2];
        assert_eq!(
            (
                odd.created,
                odd.supports_tools,
                odd.text_output,
                odd.intelligence,
                odd.context_window
            ),
            (None, None, None, None, None),
            "values of an unexpected shape are unknown, and do not break the list"
        );
    }

    #[test]
    fn anthropics_display_name_and_numeric_prices_work() {
        let json = r#"{"data":[{"id":"claude-sonnet-5","display_name":"Claude Sonnet 5","type":"model",
            "pricing":{"prompt":0.000003,"completion":0.000015}}]}"#;
        let m = &parse_models(json).unwrap()[0];
        assert_eq!(m.name.as_deref(), Some("Claude Sonnet 5"));
        assert!((m.input_per_m.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn blank_ids_are_dropped_and_garbage_is_an_error() {
        assert_eq!(
            parse_models(r#"{"data":[{"id":""},{"id":"  "},{"id":"ok"}]}"#)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(parse_models("{}").unwrap().len(), 0);
        assert!(parse_models("<html>not json</html>").is_err());
    }

    #[test]
    fn only_openrouter_hosts_are_asked_for_a_category() {
        assert!(is_openrouter("https://openrouter.ai/api/v1"));
        assert!(is_openrouter("https://eu.openrouter.ai/api/v1"));
        assert!(is_openrouter("openrouter.ai/api/v1"));
        assert!(!is_openrouter("https://api.openai.com/v1"));
        assert!(!is_openrouter("http://127.0.0.1:11434/v1"));
        assert!(!is_openrouter("https://openrouter.ai.evil.example/v1"));
        assert!(!is_openrouter("https://notopenrouter.ai/v1"));
    }
}

#[cfg(test)]
pub(crate) mod test_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned JSON answer and reports the request it was sent.
    pub(crate) async fn serve_json(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = [0u8; 4096];
            let n = sock.read(&mut scratch).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&scratch[..n]).to_string());
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }
}
