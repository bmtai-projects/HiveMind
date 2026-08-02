use std::process::Stdio;
use std::time::Duration;

use harness_config::{HookEvent, HookSpec};
use harness_types::ToolCall;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

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
///
/// `enforcement` is therefore inert here, and deliberately not treated as a
/// config error: the side effect has already happened by the time this
/// runs, so there is no call left to block, and rolling one back is not
/// something this layer can offer. A hook that must be able to *stop*
/// something has to be registered on `PreToolUse`.
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
        return on_failure(spec, "hook input could not be serialized");
    };

    // Same shell selection `run_shell` uses, imported rather than repeated:
    // this was a hardcoded `bash` until enforcement made the difference
    // load-bearing (see `harness_tools::shell_command`).
    let mut cmd = harness_tools::shell_command(&spec.command);
    // The CLI canonicalizes the workspace root, which on Windows yields a
    // `\\?\C:\...` verbatim path that `cmd.exe` refuses to start in --
    // so without this every hook fails to spawn there, and an enforcement
    // hook that cannot spawn blocks every tool call.
    cmd.current_dir(harness_tools::strip_verbatim(std::path::Path::new(
        workspace_root,
    )))
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return on_failure(spec, &format!("hook could not be started ({e})")),
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
        Ok(Err(e)) => return on_failure(spec, &format!("hook could not be waited on ({e})")),
        Err(_) => return on_failure(spec, "hook timed out"),
    };

    parse_decision(spec, &output)
}

/// The single place a hook that did not produce a usable answer is turned
/// into a decision. Advisory hooks (the default) fall through to `Allow`;
/// an `enforcement` hook denies instead, because a control that opens when
/// it breaks is not a control.
///
/// The reason string always names the hook and says the hook *failed*,
/// rather than implying the tool call was judged and rejected — otherwise
/// the model reads a broken script as a deliberate policy decision and
/// argues with it instead of surfacing it.
fn on_failure(spec: &HookSpec, what_went_wrong: &str) -> HookDecision {
    if !spec.enforcement {
        return HookDecision::Allow;
    }
    HookDecision::Deny {
        reason: format!(
            "{what_went_wrong}; '{}' is an enforcement hook, so the call was blocked rather than \
             allowed through unchecked. Fix or disable the hook -- retrying the same call will \
             fail the same way.",
            spec.name
        ),
        hook_name: spec.name.clone(),
    }
}

fn parse_decision(spec: &HookSpec, output: &std::process::Output) -> HookDecision {
    let hook_name = &spec.name;
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
        // Any other exit code means the hook itself broke, not that it
        // reached a verdict -- so this is a failure path, not an allow.
        Some(code) => on_failure(spec, &format!("hook exited with code {code}")),
        // No code at all means a signal killed it (Unix) -- also a failure,
        // never a verdict.
        None => on_failure(spec, "hook was killed by a signal"),
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
            enforcement: false,
        }
    }

    /// Same hook, but one whose failure is a denial.
    fn enforcing(command: &str) -> HookSpec {
        HookSpec {
            enforcement: true,
            ..spec(HookEvent::PreToolUse, None, command)
        }
    }

    // Hooks now spawn through `harness_tools::shell_command`, so these run
    // under `cmd.exe` on Windows and `bash` everywhere else. The two
    // commands below are the only ones in this module whose syntax differs.
    //
    // `timeout /t` is deliberately not used for the Windows sleep: it reads
    // the console directly and errors out when stdin is a pipe, which it
    // always is here.
    #[cfg(windows)]
    const SLEEP_LONGER_THAN_ANY_TIMEOUT: &str = "ping -n 6 127.0.0.1 > nul";
    #[cfg(not(windows))]
    const SLEEP_LONGER_THAN_ANY_TIMEOUT: &str = "sleep 5";

    #[cfg(windows)]
    const ECHO_JSON_DENY: &str = r#"echo {"decision":"deny","reason":"nope"}"#;
    #[cfg(not(windows))]
    const ECHO_JSON_DENY: &str = r#"echo '{"decision":"deny","reason":"nope"}'; exit 0"#;

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
        let hooks = vec![spec(HookEvent::PreToolUse, None, ECHO_JSON_DENY)];
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
            ..spec(HookEvent::PreToolUse, None, SLEEP_LONGER_THAN_ANY_TIMEOUT)
        }];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        assert_eq!(
            decision,
            HookDecision::Allow,
            "a timed-out hook must fail open"
        );
    }

    // --- enforcement hooks -------------------------------------------------
    //
    // Each of these pairs with an advisory test above that runs the *same*
    // failing command and asserts Allow. That pairing is the actual
    // guarantee: enforcement changes the outcome, and its absence leaves
    // every existing config behaving exactly as before.

    #[tokio::test]
    async fn an_enforcement_hook_that_succeeds_still_allows() {
        let decision = run_pre_tool_use(
            &[enforcing("exit 0")],
            &call("run_shell", serde_json::json!({})),
            ".",
        )
        .await;
        assert_eq!(
            decision,
            HookDecision::Allow,
            "enforcement must gate failures, not block everything"
        );
    }

    #[tokio::test]
    async fn a_broken_enforcement_hook_denies_instead_of_failing_open() {
        // Compare with `other_exit_code_fails_open`: identical command,
        // opposite outcome, and the flag is the only difference.
        let decision = run_pre_tool_use(
            &[enforcing("exit 17")],
            &call("run_shell", serde_json::json!({})),
            ".",
        )
        .await;
        match decision {
            HookDecision::Deny { reason, hook_name } => {
                assert_eq!(hook_name, "test-hook");
                // The message has to read as "the hook broke", not "your
                // call was rejected" -- otherwise the model treats a syntax
                // error as a policy it should argue with.
                assert!(reason.contains("17"), "reason should name the exit code");
                assert!(reason.contains("enforcement hook"));
            }
            HookDecision::Allow => panic!("a broken enforcement hook must not fail open"),
        }
    }

    #[tokio::test]
    async fn a_timed_out_enforcement_hook_denies() {
        let hooks = vec![HookSpec {
            timeout_ms: 100,
            ..enforcing(SLEEP_LONGER_THAN_ANY_TIMEOUT)
        }];
        let decision =
            run_pre_tool_use(&hooks, &call("run_shell", serde_json::json!({})), ".").await;
        match decision {
            HookDecision::Deny { reason, .. } => assert!(reason.contains("timed out")),
            HookDecision::Allow => panic!("a hung enforcement hook must not fail open"),
        }
    }

    #[tokio::test]
    async fn an_enforcement_hook_can_still_explicitly_allow() {
        // A hook that runs cleanly and says nothing is an allow -- proving
        // enforcement doesn't require special output to pass.
        let decision = run_pre_tool_use(
            &[enforcing("exit 0")],
            &call("edit_file", serde_json::json!({"path": "a.txt"})),
            ".",
        )
        .await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn enforcement_does_not_change_an_explicit_deny() {
        let decision = run_pre_tool_use(
            &[enforcing(ECHO_JSON_DENY)],
            &call("run_shell", serde_json::json!({})),
            ".",
        )
        .await;
        match decision {
            HookDecision::Deny { reason, .. } => {
                assert_eq!(reason, "nope", "the hook's own reason must survive intact")
            }
            HookDecision::Allow => panic!("expected the hook's explicit deny"),
        }
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
