//! Network-backed [`Embedder`] that calls HiveMind's hosted embedding
//! service instead of hashing locally. Opt-in: see [`RemoteEmbedder::from_env`].
//!
//! Everything here is written around one fact — a failure must never be
//! worse than not having used it. `embed_batch` returns `Err` rather than
//! panicking or returning junk vectors, and `SemanticSearch` responds by
//! rebuilding the entire index with the local embedder.

use std::time::Duration;

use serde::Deserialize;

use crate::semantic::Embedder;

/// Vector width the service returns. Checked on every response: a mismatch
/// means the server changed models underneath us, and silently indexing a
/// mix of widths would corrupt every later search.
const EXPECTED_DIM: usize = 768;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// The deployed hosted embedding service. Baked in so Pro mode is a single
/// switch rather than a URL a user has to discover and paste.
const DEFAULT_EMBEDDINGS_URL: &str = "https://hivemind-embeddings-549623498287.asia-south1.run.app";

pub struct RemoteEmbedder {
    client: reqwest::blocking::Client,
    url: String,
    token: String,
    model: String,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    index: usize,
    embedding: Vec<f32>,
}

impl RemoteEmbedder {
    /// Build the hosted embedder for Pro mode, or `None` when it isn't
    /// usable. `None` is never an error: the caller keeps the local
    /// embedder, which is what Standard mode uses anyway.
    ///
    /// `enabled` comes from the resolved [`Mode`], not from an env var, so
    /// the product decision lives in one place. The env vars below exist
    /// only to point a build at a different deployment (testing,
    /// self-hosting) and are not part of the normal user path.
    ///
    ///   HIVEMIND_EMBEDDINGS_URL    override the service base (no `/v1`)
    ///   HIVEMIND_EMBEDDINGS_TOKEN  override the bearer token
    ///   HIVEMIND_EMBEDDINGS_MODEL  override the model id
    pub fn for_pro_mode(enabled: bool, fallback_token: Option<&str>) -> Option<Self> {
        if !enabled {
            return None;
        }

        let base = std::env::var("HIVEMIND_EMBEDDINGS_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_EMBEDDINGS_URL.to_string());
        // Without a token there is nothing to authenticate with, so Pro
        // silently stays on the local embedder rather than failing every
        // search with a 401 -- a BYOK user has no hosted account at all.
        let token = std::env::var("HIVEMIND_EMBEDDINGS_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| fallback_token.map(str::to_string))?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .ok()?;

        Some(Self {
            client,
            url: format!("{}/v1/embeddings", base.trim_end_matches('/')),
            token,
            model: std::env::var("HIVEMIND_EMBEDDINGS_MODEL")
                .unwrap_or_else(|_| "hivemind-code".to_string()),
        })
    }
}

impl Embedder for RemoteEmbedder {
    fn dim(&self) -> usize {
        EXPECTED_DIM
    }

    fn id(&self) -> &str {
        // Includes the model, so switching models invalidates the cached
        // index instead of comparing across two vector spaces.
        &self.model
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        // Single-text path exists only to satisfy the trait; everything real
        // goes through embed_batch. A failure here yields a zero vector,
        // which scores below MIN_SCORE and simply matches nothing.
        match self.embed_batch(std::slice::from_ref(&text.to_string())) {
            Ok(mut v) if v.len() == 1 => v.remove(0),
            _ => vec![0.0; EXPECTED_DIM],
        }
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let res = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
            .send()
            .map_err(|e| format!("request failed: {e}"))?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().unwrap_or_default();
            // 402 is the one a user can actually act on, so name it plainly
            // instead of burying it in a status code.
            if status.as_u16() == 402 {
                return Err("insufficient balance for hosted embeddings".to_string());
            }
            return Err(format!(
                "http {status}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }

        let parsed: EmbedResponse = res.json().map_err(|e| format!("bad response: {e}"))?;
        if parsed.data.len() != texts.len() {
            return Err(format!(
                "expected {} embeddings, got {}",
                texts.len(),
                parsed.data.len()
            ));
        }

        // Order is not guaranteed by the wire format, so place each vector by
        // its declared index rather than trusting arrival order -- getting
        // this wrong would attach every vector to the wrong chunk.
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for datum in parsed.data {
            let slot = out
                .get_mut(datum.index)
                .ok_or_else(|| format!("index {} out of range", datum.index))?;
            if datum.embedding.len() != EXPECTED_DIM {
                return Err(format!(
                    "expected {}-dim vectors, got {}",
                    EXPECTED_DIM,
                    datum.embedding.len()
                ));
            }
            *slot = Some(datum.embedding);
        }

        out.into_iter()
            .enumerate()
            .map(|(i, v)| v.ok_or_else(|| format!("missing embedding for input {i}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_mode_never_builds_a_remote_embedder() {
        assert!(RemoteEmbedder::for_pro_mode(false, Some("hvm_live_x")).is_none());
    }

    #[test]
    fn pro_without_a_token_falls_back_instead_of_failing() {
        // A BYOK user has no hosted token; Pro must degrade to local rather
        // than 401 on every single search.
        unsafe { std::env::remove_var("HIVEMIND_EMBEDDINGS_TOKEN") };
        assert!(RemoteEmbedder::for_pro_mode(true, None).is_none());
    }

    #[test]
    fn pro_with_a_token_uses_the_built_in_url() {
        unsafe { std::env::remove_var("HIVEMIND_EMBEDDINGS_URL") };
        let e = RemoteEmbedder::for_pro_mode(true, Some("hvm_live_x"))
            .expect("pro + token should build");
        assert!(e.url.starts_with(DEFAULT_EMBEDDINGS_URL), "got {}", e.url);
        assert!(e.url.ends_with("/v1/embeddings"), "got {}", e.url);
    }
}
