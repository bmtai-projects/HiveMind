//! Pre-written, tested hooks a user can turn on by name — `hivemind hooks
//! enable no-force-push` — instead of hand-writing shell/`jq` into
//! `config.toml`.
//!
//! # Why the generated hook shells back into `hivemind` itself
//!
//! A hook's `command` is an arbitrary shell string, spawned via `bash -lc`
//! on Unix and `cmd /C` on Windows (see `harness_tools::shell_command`).
//! Writing a preset as a shell one-liner has two real problems: it needs a
//! JSON-parsing tool like `jq` on the user's `PATH` (not guaranteed,
//! especially on Windows), and it needs *two* separate implementations —
//! one bash-flavoured, one `cmd.exe`-flavoured — because the two shells
//! don't share syntax.
//!
//! Instead, `enable` writes a `command` that simply invokes
//! `hivemind hooks check <preset> [arg]`. The check itself is ordinary,
//! compiled, unit-tested Rust — it reads the exact same JSON envelope a
//! hand-written shell hook would receive on stdin, parses it with
//! `serde_json` (not string-matching), and prints the exact same
//! `{"decision":...}` protocol `harness_agent::hooks::parse_decision`
//! already expects. Every platform runs the identical logic, because it's
//! the identical binary.
//!
//! # Why these four, and not more
//!
//! Each preset here is a control someone asked for in this exact session:
//! don't let it force-push, don't let it write outside a folder, don't let
//! it touch a specific file, don't let it run the shell commands that ruin
//! your day. That is deliberately a short list. A preset that's wrong in a
//! way the user doesn't notice is worse than no preset — see
//! `crate::secrets` for the same argument applied to credential scanning —
//! so this stays small and each one is validated against real false
//! positives, rather than growing into a library nobody has exercised.

use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub enum ArgRequirement {
    None,
    /// `hint` is shown in `hivemind hooks list`, e.g. `<directory>`.
    Required {
        hint: &'static str,
    },
}

pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub arg: ArgRequirement,
    pub event: &'static str,
    pub matcher: &'static [&'static str],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "no-force-push",
        description: "Block `git push --force` (and `-f`). `--force-with-lease` is still \
                       allowed -- it's the safer form many workflows rely on.",
        arg: ArgRequirement::None,
        event: "pre_tool_use",
        matcher: &["run_shell"],
    },
    Preset {
        name: "restrict-writes-to",
        description: "Only allow write_file/edit_file inside the given directory.",
        arg: ArgRequirement::Required {
            hint: "<directory>",
        },
        event: "pre_tool_use",
        matcher: &["write_file", "edit_file"],
    },
    Preset {
        name: "protect-path",
        description: "Block write_file/edit_file to a specific file or folder, e.g. `.env` \
                       or `secrets/`.",
        arg: ArgRequirement::Required { hint: "<path>" },
        event: "pre_tool_use",
        matcher: &["write_file", "edit_file"],
    },
    Preset {
        name: "no-destructive-shell",
        description: "Block the shell commands most likely to destroy work by accident: \
                       `rm -rf`, `git reset --hard`, `git clean -f`, `mkfs`, raw disk writes. \
                       Catches the obvious footguns, not a full sandbox.",
        arg: ArgRequirement::None,
        event: "pre_tool_use",
        matcher: &["run_shell"],
    },
];

pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// What a preset decided, in the same shape the hook protocol expects.
pub enum Decision {
    Allow,
    Deny(String),
}

/// The JSON envelope a hook receives on stdin. Mirrors
/// `harness_agent::hooks::HookEnvelope` field-for-field -- kept as a
/// separate definition rather than shared, since this crate has no
/// dependency on `harness-agent` and the two are independently pinned to
/// the same wire spec already (see that module's own doc comment on why it
/// avoids a shared typed enum in the first place).
#[derive(Deserialize)]
struct Envelope {
    tool_name: String,
    /// JSON *text*, not a parsed value -- this is a string containing the
    /// tool call's arguments as JSON, double-encoded exactly as the real
    /// hook envelope sends it.
    tool_args: String,
}

#[derive(Deserialize)]
struct PathArg {
    path: String,
}

#[derive(Deserialize)]
struct CommandArg {
    command: String,
}

/// Evaluate `preset` against one hook invocation. `envelope_json` is stdin,
/// verbatim. Unknown preset names or unparseable input return `Err` rather
/// than `Decision` -- the caller (`main.rs`'s `hooks check`) turns that into
/// a nonzero exit with no valid `{"decision":...}` output, which is exactly
/// what should happen: a misconfigured preset falls through to the hook's
/// own `enforcement` setting (`harness_agent::hooks::on_failure`) instead
/// of this module inventing its own separate fallback rule.
pub fn evaluate(preset: &str, arg: Option<&str>, envelope_json: &str) -> Result<Decision, String> {
    let envelope: Envelope =
        serde_json::from_str(envelope_json).map_err(|e| format!("bad envelope: {e}"))?;

    match preset {
        "no-force-push" => Ok(no_force_push(&envelope)),
        "restrict-writes-to" => {
            let dir = arg.ok_or("restrict-writes-to needs a directory argument")?;
            Ok(restrict_writes_to(&envelope, dir))
        }
        "protect-path" => {
            let path = arg.ok_or("protect-path needs a path argument")?;
            Ok(protect_path(&envelope, path))
        }
        "no-destructive-shell" => Ok(no_destructive_shell(&envelope)),
        other => Err(format!("unknown preset {other:?}")),
    }
}

fn shell_command_text(envelope: &Envelope) -> Option<String> {
    if envelope.tool_name != "run_shell" {
        return None;
    }
    serde_json::from_str::<CommandArg>(&envelope.tool_args)
        .ok()
        .map(|a| a.command)
}

fn no_force_push(envelope: &Envelope) -> Decision {
    let Some(cmd) = shell_command_text(envelope) else {
        return Decision::Allow;
    };
    if !cmd.contains("push") {
        return Decision::Allow;
    }
    let has_force = cmd
        .split_whitespace()
        .any(|tok| tok == "--force" || tok == "-f");
    let has_safe_lease = cmd.contains("--force-with-lease");
    if has_force && !has_safe_lease {
        Decision::Deny(format!(
            "'{cmd}' looks like a force push. If you really mean it, run it yourself outside \
             hivemind, or use --force-with-lease which this preset allows."
        ))
    } else {
        Decision::Allow
    }
}

/// Destructive-shell substrings, checked as whole-word-ish fragments rather
/// than a full parser -- same tradeoff as `crate::secrets`: this catches the
/// common, obvious cases and says so, rather than pretending to be exhaustive.
const DESTRUCTIVE_FRAGMENTS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "git reset --hard",
    "git clean -fd",
    "git clean -df",
    "git clean -xfd",
    "mkfs",
    "dd if=",
    "> /dev/sd",
    "> /dev/nvme",
    "drop table",
    "drop database",
];

fn no_destructive_shell(envelope: &Envelope) -> Decision {
    let Some(cmd) = shell_command_text(envelope) else {
        return Decision::Allow;
    };
    let lower = cmd.to_ascii_lowercase();
    match DESTRUCTIVE_FRAGMENTS.iter().find(|f| lower.contains(*f)) {
        Some(hit) => Decision::Deny(format!(
            "'{cmd}' contains '{hit}', which this preset blocks as one of the commands most \
             likely to destroy work by accident. Run it yourself outside hivemind if you're sure."
        )),
        None => Decision::Allow,
    }
}

/// True when `path`, split into components, starts with every component of
/// `prefix`. Component-wise on purpose: a naive string-prefix check would
/// let `srcish/x` pass a `src` restriction.
fn path_starts_with(path: &str, prefix: &str) -> bool {
    let mut p = std::path::Path::new(path).components();
    let mut want = std::path::Path::new(prefix).components();
    loop {
        match (want.next(), p.next()) {
            (None, _) => return true,
            (Some(w), Some(got)) if w == got => continue,
            _ => return false,
        }
    }
}

fn write_target_path(envelope: &Envelope) -> Option<String> {
    if !matches!(envelope.tool_name.as_str(), "write_file" | "edit_file") {
        return None;
    }
    serde_json::from_str::<PathArg>(&envelope.tool_args)
        .ok()
        .map(|a| a.path)
}

fn restrict_writes_to(envelope: &Envelope, dir: &str) -> Decision {
    let Some(path) = write_target_path(envelope) else {
        return Decision::Allow;
    };
    if path_starts_with(&path, dir) {
        Decision::Allow
    } else {
        Decision::Deny(format!(
            "{path} is outside {dir}, and this session is restricted to writing inside {dir}."
        ))
    }
}

fn protect_path(envelope: &Envelope, protected: &str) -> Decision {
    let Some(path) = write_target_path(envelope) else {
        return Decision::Allow;
    };
    if path_starts_with(&path, protected) {
        Decision::Deny(format!("{path} is protected and cannot be written to."))
    } else {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(tool_name: &str, args: serde_json::Value) -> String {
        serde_json::json!({
            "event": "pre_tool_use",
            "tool_name": tool_name,
            "tool_args": args.to_string(),
            "workspace_root": "/tmp/ws",
        })
        .to_string()
    }

    fn is_allow(d: Decision) -> bool {
        matches!(d, Decision::Allow)
    }
    fn deny_reason(d: Decision) -> String {
        match d {
            Decision::Deny(r) => r,
            Decision::Allow => panic!("expected Deny"),
        }
    }

    // --- no-force-push ------------------------------------------------

    #[test]
    fn an_ordinary_push_is_allowed() {
        let e = env(
            "run_shell",
            serde_json::json!({"command": "git push origin main"}),
        );
        assert!(is_allow(evaluate("no-force-push", None, &e).unwrap()));
    }

    #[test]
    fn a_force_push_is_denied() {
        let e = env(
            "run_shell",
            serde_json::json!({"command": "git push --force origin main"}),
        );
        let r = deny_reason(evaluate("no-force-push", None, &e).unwrap());
        assert!(r.contains("force push"));
    }

    #[test]
    fn short_flag_force_push_is_denied() {
        let e = env("run_shell", serde_json::json!({"command": "git push -f"}));
        assert!(!is_allow(evaluate("no-force-push", None, &e).unwrap()));
    }

    #[test]
    fn force_with_lease_is_allowed() {
        // The one deliberate exception: --force-with-lease is the form
        // many real workflows consider safe.
        let e = env(
            "run_shell",
            serde_json::json!({"command": "git push --force-with-lease origin main"}),
        );
        assert!(is_allow(evaluate("no-force-push", None, &e).unwrap()));
    }

    #[test]
    fn a_non_shell_tool_is_untouched_by_the_push_preset() {
        let e = env("edit_file", serde_json::json!({"path": "a.rs"}));
        assert!(is_allow(evaluate("no-force-push", None, &e).unwrap()));
    }

    #[test]
    fn a_command_that_merely_mentions_force_is_not_a_false_positive() {
        // "force" the word, unrelated to a push flag, must not trip this.
        let e = env(
            "run_shell",
            serde_json::json!({"command": "echo 'do not force anything here'"}),
        );
        assert!(is_allow(evaluate("no-force-push", None, &e).unwrap()));
    }

    // --- no-destructive-shell ------------------------------------------

    #[test]
    fn rm_rf_is_denied() {
        let e = env("run_shell", serde_json::json!({"command": "rm -rf build/"}));
        let r = deny_reason(evaluate("no-destructive-shell", None, &e).unwrap());
        assert!(r.contains("rm -rf"));
    }

    #[test]
    fn git_reset_hard_is_denied() {
        let e = env(
            "run_shell",
            serde_json::json!({"command": "git reset --hard HEAD~3"}),
        );
        assert!(!is_allow(
            evaluate("no-destructive-shell", None, &e).unwrap()
        ));
    }

    #[test]
    fn an_ordinary_rm_is_allowed() {
        let e = env(
            "run_shell",
            serde_json::json!({"command": "rm build/output.log"}),
        );
        assert!(is_allow(
            evaluate("no-destructive-shell", None, &e).unwrap()
        ));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let e = env("run_shell", serde_json::json!({"command": "RM -RF /tmp/x"}));
        assert!(!is_allow(
            evaluate("no-destructive-shell", None, &e).unwrap()
        ));
    }

    // --- restrict-writes-to ---------------------------------------------

    #[test]
    fn a_write_inside_the_directory_is_allowed() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "src/main.rs", "content": "x"}),
        );
        assert!(is_allow(
            evaluate("restrict-writes-to", Some("src"), &e).unwrap()
        ));
    }

    #[test]
    fn a_write_outside_the_directory_is_denied() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "config/secrets.toml", "content": "x"}),
        );
        let r = deny_reason(evaluate("restrict-writes-to", Some("src"), &e).unwrap());
        assert!(r.contains("config/secrets.toml"));
    }

    #[test]
    fn a_similarly_named_directory_does_not_falsely_match() {
        // "srcish/" starts with the string "src" but is not inside it --
        // must not pass a component-wise prefix check.
        let e = env(
            "write_file",
            serde_json::json!({"path": "srcish/x.rs", "content": "x"}),
        );
        assert!(!is_allow(
            evaluate("restrict-writes-to", Some("src"), &e).unwrap()
        ));
    }

    #[test]
    fn a_nested_write_inside_the_directory_is_allowed() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "src/nested/deep/file.rs", "content": "x"}),
        );
        assert!(is_allow(
            evaluate("restrict-writes-to", Some("src"), &e).unwrap()
        ));
    }

    #[test]
    fn edit_file_is_covered_the_same_as_write_file() {
        let e = env(
            "edit_file",
            serde_json::json!({"path": "outside.rs", "old_string": "a", "new_string": "b"}),
        );
        assert!(!is_allow(
            evaluate("restrict-writes-to", Some("src"), &e).unwrap()
        ));
    }

    #[test]
    fn a_read_only_tool_is_unaffected() {
        let e = env(
            "read_file",
            serde_json::json!({"path": "config/secrets.toml"}),
        );
        assert!(is_allow(
            evaluate("restrict-writes-to", Some("src"), &e).unwrap()
        ));
    }

    #[test]
    fn missing_argument_is_an_error_not_a_silent_allow() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "x", "content": "y"}),
        );
        assert!(evaluate("restrict-writes-to", None, &e).is_err());
    }

    // --- protect-path -----------------------------------------------------

    #[test]
    fn writing_the_protected_file_is_denied() {
        let e = env(
            "write_file",
            serde_json::json!({"path": ".env", "content": "x"}),
        );
        let r = deny_reason(evaluate("protect-path", Some(".env"), &e).unwrap());
        assert!(r.contains(".env"));
    }

    #[test]
    fn writing_anything_else_is_allowed() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "src/main.rs", "content": "x"}),
        );
        assert!(is_allow(
            evaluate("protect-path", Some(".env"), &e).unwrap()
        ));
    }

    #[test]
    fn protecting_a_directory_covers_everything_inside_it() {
        let e = env(
            "write_file",
            serde_json::json!({"path": "secrets/db.json", "content": "x"}),
        );
        assert!(!is_allow(
            evaluate("protect-path", Some("secrets"), &e).unwrap()
        ));
    }

    // --- misconfiguration -------------------------------------------------

    #[test]
    fn an_unknown_preset_name_is_an_error() {
        let e = env("run_shell", serde_json::json!({"command": "ls"}));
        assert!(evaluate("not-a-real-preset", None, &e).is_err());
    }

    #[test]
    fn malformed_envelope_json_is_an_error_not_a_panic() {
        assert!(evaluate("no-force-push", None, "not json").is_err());
    }

    #[test]
    fn every_preset_name_is_findable_by_find() {
        for p in PRESETS {
            assert!(find(p.name).is_some());
        }
        assert!(find("nonexistent").is_none());
    }
}
