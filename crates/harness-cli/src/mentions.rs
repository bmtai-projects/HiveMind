//! Expands `@path` mentions in user input into inline file-content blocks,
//! so the model gets the file's content directly instead of needing a
//! `read_file` round-trip. Pure string processing + filesystem reads — no
//! terminal interaction, so it behaves identically whether the input came
//! from the interactive REPL or a headless `-p` prompt.

use harness_tools::Workspace;

const MAX_MENTION_BYTES: usize = 60_000;

/// Scan `input` for `@relative/path` tokens that resolve to real files
/// under `workspace`, and append their content as labeled blocks after the
/// original text. A mention that doesn't resolve to a real file (typo,
/// outside the workspace, or just an `@handle`/email that isn't a path) is
/// left untouched as plain text — never an error.
pub fn expand_mentions(input: &str, workspace: &Workspace) -> String {
    let mut resolved: Vec<(String, String)> = Vec::new();

    for token in input.split_whitespace() {
        let Some(candidate) = extract_mention(token) else {
            continue;
        };
        if resolved.iter().any(|(p, _)| p == candidate) {
            continue; // already inlined this path
        }
        let Ok(path) = workspace.resolve(candidate) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let content = if bytes.len() > MAX_MENTION_BYTES {
            format!(
                "{}\n\n[truncated: {} bytes total]",
                String::from_utf8_lossy(&bytes[..MAX_MENTION_BYTES]),
                bytes.len()
            )
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        resolved.push((candidate.to_string(), content));
    }

    if resolved.is_empty() {
        return input.to_string();
    }

    let mut out = input.to_string();
    out.push_str("\n\n<mentioned-files>\n");
    for (path, content) in &resolved {
        out.push_str(&format!("<file path=\"{path}\">\n{content}\n</file>\n"));
    }
    out.push_str("</mentioned-files>");
    out
}

/// `@src/main.rs,` -> `Some("src/main.rs")`. `foo@bar.com` -> `None` (the
/// `@` isn't at the start of the token, so it's not a mention). Bare `@`
/// -> `None`.
fn extract_mention(token: &str) -> Option<&str> {
    let rest = token.strip_prefix('@')?;
    let trimmed = rest.trim_end_matches([',', '.', ';', ':', ')', ']', '!', '?']);
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn test_workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("hivemind_mentions_test_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    #[test]
    fn expands_a_real_file_mention() {
        let ws = test_workspace("expands_real");
        fs::write(ws.root.join("hello.txt"), "hello world").unwrap();
        let out = expand_mentions("summarize @hello.txt please", &ws);
        assert!(out.starts_with("summarize @hello.txt please"));
        assert!(out.contains("hello world"));
        assert!(out.contains("path=\"hello.txt\""));
    }

    #[test]
    fn leaves_nonexistent_mentions_untouched() {
        let ws = test_workspace("nonexistent");
        let out = expand_mentions("check @nope.txt", &ws);
        assert_eq!(out, "check @nope.txt");
    }

    #[test]
    fn strips_trailing_punctuation_from_mention() {
        let ws = test_workspace("punct");
        fs::write(ws.root.join("a.rs"), "fn main() {}").unwrap();
        let out = expand_mentions("look at @a.rs, then fix it", &ws);
        assert!(out.contains("fn main() {}"));
        assert!(out.contains("path=\"a.rs\""));
    }

    #[test]
    fn does_not_expand_email_like_tokens() {
        let ws = test_workspace("email");
        let out = expand_mentions("contact foo@bar.com about this", &ws);
        assert_eq!(out, "contact foo@bar.com about this");
    }

    #[test]
    fn no_mentions_returns_input_unchanged() {
        let ws = test_workspace("none");
        let out = expand_mentions("just a normal message", &ws);
        assert_eq!(out, "just a normal message");
    }

    #[test]
    fn escaping_workspace_root_is_ignored() {
        let ws = test_workspace("escape");
        let out = expand_mentions("read @../../../etc/passwd", &ws);
        assert_eq!(out, "read @../../../etc/passwd");
    }

    #[test]
    fn duplicate_mentions_inline_once() {
        let ws = test_workspace("dup");
        fs::write(ws.root.join("x.txt"), "xcontent").unwrap();
        let out = expand_mentions("@x.txt and again @x.txt", &ws);
        assert_eq!(out.matches("xcontent").count(), 1);
    }
}
