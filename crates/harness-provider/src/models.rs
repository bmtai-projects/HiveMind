//! Asking a provider which models it offers, and what it says about each.
//!
//! OpenAI-style servers answer `GET {base}/models` with `{"data": [{"id": ..}]}`.
//! OpenRouter adds a context size, per-token prices and, for reasoning models,
//! the exact effort levels each accepts. Anthropic answers with `display_name`.
//! Everything beyond `id` is optional, and a missing field means "unknown".

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

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
}

pub type ModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<RemoteModel>, ProviderError>> + Send + 'a>>;

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    pricing: Option<RawPricing>,
    #[serde(default)]
    reasoning: Option<RawReasoning>,
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

pub(crate) fn parse_models(json: &str) -> Result<Vec<RemoteModel>, ProviderError> {
    let list: ModelList = serde_json::from_str(json)
        .map_err(|e| ProviderError::Request(format!("the model list was not understood: {e}")))?;
    Ok(list
        .data
        .into_iter()
        .filter(|m| !m.id.trim().is_empty())
        .map(|m| RemoteModel {
            id: m.id,
            name: m.name.or(m.display_name).filter(|n| !n.trim().is_empty()),
            context_window: m.context_length.filter(|n| *n > 0),
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
                "HTTP/1.1 {status}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }
}
