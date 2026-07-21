//! Manual smoke / embedder-quality harness: run `semantic_search` over the
//! real HiveMind repo and eyeball the rankings. `#[ignore]`d so it never runs
//! in CI (it depends on the repo's own file layout); it's here to (a) prove the
//! pipeline works end-to-end on real code and (b) give an A/B when someone
//! swaps the default [`HashingEmbedder`] for a neural one — rerun and compare.
//!
//! Run with:
//!   cargo test -p harness-tools --test real_repo_smoke -- --ignored --nocapture

use harness_tools::{SemanticSearch, Tool, Workspace};
use serde_json::value::RawValue;

async fn top_hits(tool: &SemanticSearch, query: &str, scope: Option<&str>, k: usize) -> String {
    let mut obj = serde_json::json!({ "query": query, "top_k": k });
    if let Some(s) = scope {
        obj["path"] = serde_json::json!(s);
    }
    let args = RawValue::from_string(obj.to_string()).unwrap();
    tool.execute(&args).await.unwrap()
}

#[tokio::test]
#[ignore = "manual smoke against the real repo"]
async fn semantic_search_on_the_real_repo() {
    let repo_root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let tool = SemanticSearch::new(Workspace::new(repo_root));

    // Print a spread of whole-repo concept queries for eyeballing quality.
    for q in [
        "retry a failed request with exponential backoff on rate limits",
        "summarize old conversation turns to save context",
        "ask the user to approve a shell command before running it",
        "keep the prompt prefix stable so the cache hits",
    ] {
        println!("\n### query: {q}\n{}", top_hits(&tool, q, None, 3).await);
    }

    // Assertions are scoped to the owning crate — deterministic, and immune to
    // this meta-file's own query text polluting a whole-repo search. Within the
    // provider crate a backoff query must surface retry.rs; within the agent
    // crate a summary query must surface compaction.rs. (The synonym-heavy
    // queries above, where a neural embedder would pull ahead, are printed for
    // inspection but deliberately not asserted.)
    let retry = top_hits(
        &tool,
        "retry a failed request with exponential backoff",
        Some("crates/harness-provider"),
        1,
    )
    .await;
    assert!(
        retry.contains("retry.rs"),
        "expected retry.rs, got:\n{retry}"
    );

    let compact = top_hits(
        &tool,
        "summarize old conversation turns into one summary",
        Some("crates/harness-agent"),
        1,
    )
    .await;
    assert!(
        compact.contains("compaction.rs"),
        "expected compaction.rs, got:\n{compact}"
    );
}
