use std::process::Stdio;
use std::time::Duration;

use harness_config::{HookEvent, HookSpec};
use harness_types::ToolCall;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Cap on the serialized size of a tool call's args/result embedded in a
/// hook's stdin envelope, matching grok-build's own constant
/// (`event.rs::MAX_PAYLOAD_SIZE`) for the same reason: don't let a giant
/// `write_file` content blow up a hook's stdin.
const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// Matches Claude Code's own hook convention (and grok-build's), so a hook
/// script written for one can plausibly be reused for the other.
const DENY_EXIT_CODE: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny { reason: String, hook_name: String },
}

#[derive(Serialize)]
struct HookEnvelope<'a> {
    event: &'a str,
    tool_name: &'a str,
    tool_args: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_result: Option<&'a str>,
    workspace_root: &'a str,
}

#[derive(Deserialize)]
struct HookOutput {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

fn matches(spec: &HookSpec, event: HookEvent, tool_name: &str) -> bool {
    spec.event == event
        && match &spec.matcher {
            None => true,
            Some(names) => names.iter().any(|n| n == tool_name),
        }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_PAYLOAD_BYTES {
        return s.to_string();
    }
    let mut idx = MAX_PAYLOAD_BYTES;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut out = s[..idx].to_string();
    out.push_str("\n[truncated]");
    out
}

/// Runs every `PreToolUse` hook matching `call`, in declared order. The
/// first `Deny` short-circuits; if none deny (or none match), the call is
/// allowed.
pub async fn run_pre_tool_use(
    hooks: &[HookSpec],
    call: &ToolCall,
    workspace_root: &str,
) -> HookDecision {
    for spec in hooks {
        if !matches(spec, HookEvent::PreToolUse, &call.name) {
            continue;
        }
        let decision = run_one(spec, HookEvent::PreToolUse, call, None, workspace_root).await;
        if matches!(decision, HookDecision::Deny { .. }) {
            return decision;
        }
    }
    HookDecision::Allow
}

/// Runs every `PostToolUse` hook matching `call`, purely observationally —
/// the return value is discarded by design (see module docs).
pub async fn run_post_tool_use(
    hooks: &[HookSpec],
    call: &ToolCall,
    result: &str,
    workspace_root: &str,
) {
    for spec in hooks {
        if !matches(spec, HookEvent::PostToolUse, &call.name) {
            continue;
        }
        let _ = run_one(
            spec,
            HookEvent::PostToolUse,
            call,
            Some(result),
            workspace_root,
        )
        .await;
    }
}

async fn run_one(
    spec: &HookSpec,
    event: HookEvent,
    call: &ToolCall,
    result: Option<&str>,
    workspace_root: &str,
) -> HookDecision {
    let args_json = truncate(call.args.get());
    let result_owned = result.map(truncate);
    let envelope = HookEnvelope {
        event: match event {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
        },
        tool_name: &call.name,
        tool_args: &args_json,
        tool_result: result_owned.as_deref(),
        workspace_root,
    };
    let Ok(stdin_json) = serde_json::to_string(&envelope) else {
        return HookDecision::Allow; // couldn't even build the envelope -- fail open
    };

    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(&spec.command)
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return HookDecision::Allow, // couldn't even spawn -- fail open
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_json.as_bytes()).await;
        // Explicit drop closes the pipe so the hook's read on stdin (if
        // any) sees EOF instead of hanging until the timeout.
        drop(stdin);
    }

    let run = child.wait_with_output();
    let output = match tokio::time::timeout(Duration::from_millis(spec.timeout_ms), run).await {
        Ok(Ok(output)) => output,
        _ => return HookDecision::Allow, // timed out, or the wait itself errored -- fail open
    };

    parse_decision(&output, &spec.name)
}

fn parse_decision(output: &std::process::Output, hook_name: &str) -> HookDecision {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(parsed) = serde_json::from_str::<HookOutput>(stdout.trim()) {
        return if parsed.decision.eq_ignore_ascii_case("deny") {
            HookDecision::Deny {
                reason: parsed
                    .reason
                    .unwrap_or_else(|| "denied by hook".to_string()),
                hook_name: hook_name.to_string(),
            }
        } else {
            HookDecision::Allow
        };
    }

    match output.status.code() {
        Some(0) => HookDecision::Allow,
        Some(DENY_EXIT_CODE) => HookDecision::Deny {
            reason: format!("denied by hook '{hook_name}' (exit code {DENY_EXIT_CODE})"),
            hook_name: hook_name.to_string(),
        },
        _ => HookDecision::Allow, // the hook itself failed -- fail open, not a deny
    }
}

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;

    use super::*;

    fn call(name: &str, json: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            args: RawValue::from_string(json.to_string()).unwrap(),
        }
    }

    fn spec(event: HookEvent, matcher: Option<Vec<&str>>, command: &str) -> HookSpec {
        HookSpec {
            name: "test-hook".to_string(),
            event,
            matcher: matcher.map(|v| v.into_iter().map(String::from).collect()),
            command: command.to_string(),
            timeout_ms: 2_000,
        }
    }

    #[tokio::test]
    async fn exit_zero_allows() {
        let hooks = vec![spec(HookEvent::PreToolUse, None, "exit 0")];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn exit_two_denies() {
        let hooks = vec![spec(HookEvent::PreToolUse, None, "exit 2")];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        match decision {
            HookDecision::Deny { hook_name, .. } => assert_eq!(hook_name, "test-hook"),
            HookDecision::Allow => panic!("expected Deny"),
        }
    }

    #[tokio::test]
    async fn other_exit_code_fails_open() {
        let hooks = vec![spec(HookEvent::PreToolUse, None, "exit 17")];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        assert_eq!(
            decision,
            HookDecision::Allow,
            "a broken hook must fail open, not deny"
        );
    }

    #[tokio::test]
    async fn structured_json_deny_takes_precedence_over_exit_code() {
        let hooks = vec![spec(
            HookEvent::PreToolUse,
            None,
            r#"echo '{"decision":"deny","reason":"nope"}'; exit 0"#,
        )];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        match decision {
            HookDecision::Deny { reason, .. } => assert_eq!(reason, "nope"),
            HookDecision::Allow => panic!("expected Deny from structured JSON output"),
        }
    }

    #[tokio::test]
    async fn timeout_fails_open() {
        let hooks = vec![HookSpec {
            timeout_ms: 100,
            ..spec(HookEvent::PreToolUse, None, "sleep 5; exit 2")
        }];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        assert_eq!(
            decision,
            HookDecision::Allow,
            "a timed-out hook must fail open"
        );
    }

    #[tokio::test]
    async fn non_matching_tool_name_never_runs() {
        // If this ran, it would deny -- so Allow here proves the matcher
        // correctly skipped it, not that the command coincidentally passed.
        let hooks = vec![spec(
            HookEvent::PreToolUse,
            Some(vec!["run_shell"]),
            "exit 2",
        )];
        let decision =
            run_pre_tool_use(&hooks, &call("edit_file", serde_json::json!({})), ".").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn wildcard_matcher_applies_to_every_tool() {
        let hooks = vec![spec(HookEvent::PreToolUse, None, "exit 2")];
        let decision =
            run_pre_tool_use(&hooks, &call("edit_file", serde_json::json!({})), ".").await;
        assert!(matches!(decision, HookDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn post_tool_use_hook_never_matches_pre_tool_use_event() {
        // A PostToolUse-scoped hook that would deny must not be consulted
        // by run_pre_tool_use at all.
        let hooks = vec![spec(HookEvent::PostToolUse, None, "exit 2")];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn envelope_reaches_the_hook_on_stdin() {
        // Round-trip: a hook that greps its own stdin for the tool name
        // denies only if the envelope was actually delivered correctly.
        let hooks = vec![spec(
            HookEvent::PreToolUse,
            None,
            r#"grep -q '"tool_name":"run_shell"' && exit 2 || exit 0"#,
        )];
        let decision = run_pre_tool_use(
            &hooks,
            &call("run_shell", serde_json::json!({"command":"ls"})),
            ".",
        )
        .await;
        assert!(matches!(decision, HookDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn first_denying_hook_short_circuits_later_ones() {
        // Two hooks: the first denies. If the second ran too, we couldn't
        // tell from this assertion alone, so pair it with a distinct
        // reason check to prove which one actually fired.
        let hooks = vec![
            spec(
                HookEvent::PreToolUse,
                None,
                r#"echo '{"decision":"deny","reason":"first"}'"#,
            ),
            spec(
                HookEvent::PreToolUse,
                None,
                r#"echo '{"decision":"deny","reason":"second"}'"#,
            ),
        ];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        match decision {
            HookDecision::Deny { reason, .. } => assert_eq!(reason, "first"),
            HookDecision::Allow => panic!("expected Deny"),
        }
    }
}
