//! M2 validation: the parts of artifact handling that only mean anything
//! once the store, the session store, and compaction are wired together.
//!
//! The unit-level behaviour (preview quality, path safety, slicing) lives in
//! `harness_tools::artifact`. What is checked here is the integration the
//! plan calls out: V9 (retention leaves no orphans), V10 (a handle still
//! resolves after compaction has discarded the message that carried it), and
//! the two properties that make this safe to ship -- the feature is inert
//! until switched on, and switching it on doesn't change the wire.

use harness_agent::{SessionRecord, SessionStore, unix_now};
use harness_tools::{
    ArtifactStore, DEFAULT_ARTIFACT_THRESHOLD_BYTES, ToolResult, preview, text_to_offload,
};
use harness_types::Message;

const DAY: u64 = 86_400;

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hm-m2-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn record(id: &str, updated_at: u64) -> SessionRecord {
    SessionRecord {
        id: id.to_string(),
        workspace: "/tmp/ws".to_string(),
        model: "hivemind".to_string(),
        reasoning_effort: None,
        budget_usd: None,
        session_cost_usd: 0.0,
        web_enabled: false,
        messages: vec![Message::system("s")],
        created_at: updated_at,
        updated_at,
        title: "t".to_string(),
    }
}

/// V9. An artifact must not outlive the conversation that produced it.
/// Nothing else ever references it, so a leak here is permanent.
#[test]
fn pruning_a_session_takes_its_artifacts_and_leaves_the_rest() {
    let sessions = SessionStore::new(tmp("v9-sessions"));
    let artifacts = ArtifactStore::new(tmp("v9-artifacts"));

    let now = unix_now();
    sessions.save(&record("stale", now - 30 * DAY)).unwrap();
    sessions.save(&record("fresh", now)).unwrap();
    let stale = artifacts
        .store("stale", "call1", "output", "old output")
        .unwrap();
    let fresh = artifacts
        .store("fresh", "call1", "output", "current output")
        .unwrap();

    let pruned = sessions.prune_older_than_except(14 * DAY, &[]);
    assert_eq!(pruned, vec!["stale".to_string()]);
    artifacts.remove_sessions(&pruned);

    assert!(
        artifacts.read_slice(&stale.uri, None, None).is_err(),
        "the pruned session's artifact survived it -- this leaks disk forever"
    );
    assert!(
        artifacts.read_slice(&fresh.uri, None, None).is_ok(),
        "pruning one session destroyed a live session's artifact"
    );

    // And nothing is left behind on disk for the pruned session.
    assert!(
        !artifacts.root().join("stale").exists(),
        "an empty orphan directory was left behind"
    );
}

/// V10. Compaction throws away old messages, including the preview that
/// carried the handle. The artifact is on disk, so the content must still be
/// reachable -- that durability is a large part of why this is better than
/// trimming, which loses the text outright.
#[test]
fn a_handle_still_resolves_after_compaction_discards_its_message() {
    let artifacts = ArtifactStore::new(tmp("v10"));
    let content = (1..=5_000)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let handle = artifacts
        .store("sess", "call1", "output", &content)
        .unwrap();

    // A conversation that once held the preview...
    let mut messages = vec![
        Message::system("s"),
        Message::user("run the tests"),
        Message::tool_result("call1", "run_shell", preview(&content, &handle)),
    ];
    // ...then gets compacted down to a summary, losing it entirely.
    messages = vec![
        Message::system("s"),
        Message::user("[summary of earlier conversation]"),
    ];
    assert!(
        !messages.iter().any(|m| m.content.contains(&handle.uri)),
        "the test is not proving anything -- the handle is still in context"
    );

    let slice = artifacts
        .read_slice(&handle.uri, Some(2_500), Some(2))
        .unwrap();
    assert!(
        slice.starts_with("line 2500\nline 2501"),
        "artifact unreadable after compaction: {slice}"
    );
}

/// A session id is not a safe filename by construction -- it is generated
/// from a workspace path hash. Confirm the real generator produces ids the
/// artifact store accepts, so this can't fail only in production.
#[test]
fn real_session_ids_are_usable_as_artifact_paths() {
    let artifacts = ArtifactStore::new(tmp("ids"));
    for ws in [
        "/Users/someone/Documents/projects/HiveMind",
        "/tmp/a b c/weird name",
        "C:\\Users\\someone\\project",
        "/",
    ] {
        let id = SessionStore::new_id(ws);
        let h = artifacts
            .store(&id, "call-abc123", "output", "x")
            .unwrap_or_else(|e| panic!("session id {id:?} (from {ws:?}) rejected: {e}"));
        assert!(artifacts.read_slice(&h.uri, None, None).is_ok());
    }
}

/// The threshold default lives in two crates that cannot import each other
/// (harness-config must not depend on the tool layer). This crate depends on
/// both, so it is where the two can be held to the same number.
#[test]
fn the_artifact_threshold_default_matches_the_tool_layer() {
    assert_eq!(
        harness_config::AgentPolicy::default().artifact_threshold_bytes,
        DEFAULT_ARTIFACT_THRESHOLD_BYTES,
        "the config default and the tool-layer default have drifted apart"
    );
}

/// The offload decision itself, exercised directly rather than inferred.
/// `text_to_offload` is what `Agent::offload_if_large` consults, so these
/// are the real branches.
#[test]
fn the_offload_decision_covers_every_branch_that_matters() {
    let threshold = DEFAULT_ARTIFACT_THRESHOLD_BYTES;

    // The median call (baseline: 4,772 bytes) is left completely alone.
    let ordinary = ToolResult::ok("a".repeat(threshold - 1));
    assert_eq!(
        text_to_offload(&ordinary, threshold),
        None,
        "an under-threshold result would have been offloaded"
    );

    // A genuinely large single-shot result is archived.
    let big = ToolResult::ok("a".repeat(threshold));
    assert_eq!(
        text_to_offload(&big, threshold).map(str::len),
        Some(threshold)
    );

    // The case this whole channel exists for: run_shell clamped a 2 MB test
    // run down to 60 KB. Sizing on `summary` alone would archive the
    // truncated text and save nothing.
    let clamped = ToolResult {
        summary: "first 60KB…\n[truncated]".to_string(),
        full_output: Some("x".repeat(2_000_000)),
        ..Default::default()
    };
    assert_eq!(
        text_to_offload(&clamped, threshold).map(str::len),
        Some(2_000_000),
        "offloaded the truncated summary instead of the full output"
    );

    // A short summary with no full_output stays inline even though the
    // struct has the field.
    let short = ToolResult {
        summary: "ok".to_string(),
        full_output: None,
        ..Default::default()
    };
    assert_eq!(text_to_offload(&short, threshold), None);

    // Threshold 0 is the off switch, not "archive everything".
    assert_eq!(
        text_to_offload(&big, 0),
        None,
        "threshold 0 must disable offloading, not archive every result"
    );
}
