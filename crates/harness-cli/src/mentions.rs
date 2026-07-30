//! Expands `@path` mentions in user input into inline file-content blocks,
//! so the model gets the file's content directly instead of needing a
//! `read_file` round-trip. Pure string processing + filesystem reads — no
//! terminal interaction, so it behaves identically whether the input came
//! from the interactive REPL or a headless `-p` prompt.

use std::path::Path;

use harness_tools::Workspace;

const MAX_MENTION_BYTES: usize = 60_000;
const MAX_DIR_ENTRIES: usize = 400;

enum Mentioned {
    File(String),
    Dir(String),
}

/// Scan `input` for `@relative/path` tokens that resolve under `workspace`,
/// and append what they point at as labeled blocks after the original text:
/// a file's content, or a directory's recursive file listing. A mention that
/// doesn't resolve (typo, outside the workspace, or just an `@handle`/email
/// that isn't a path) is left untouched as plain text — never an error.
pub fn expand_mentions(input: &str, workspace: &Workspace) -> String {
    let mut resolved: Vec<(String, Mentioned)> = Vec::new();

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
        if metadata.is_dir() {
            resolved.push((candidate.to_string(), Mentioned::Dir(dir_listing(&path))));
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if is_binary(&bytes) {
            // A JAR/class/image mention has no useful text form; dumping its
            // lossy-UTF8 decode into the prompt would just be noise.
            continue;
        }
        let content = if bytes.len() > MAX_MENTION_BYTES {
            format!(
                "{}\n\n[truncated: {} bytes total]",
                String::from_utf8_lossy(&bytes[..MAX_MENTION_BYTES]),
                bytes.len()
            )
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        resolved.push((candidate.to_string(), Mentioned::File(content)));
    }

    if resolved.is_empty() {
        return input.to_string();
    }

    let mut out = input.to_string();
    out.push_str("\n\n<mentioned-files>\n");
    for (path, item) in &resolved {
        match item {
            Mentioned::File(content) => {
                out.push_str(&format!("<file path=\"{path}\">\n{content}\n</file>\n"));
            }
            Mentioned::Dir(listing) => {
                out.push_str(&format!(
                    "<directory path=\"{path}\">\n{listing}\n</directory>\n"
                ));
            }
        }
    }
    out.push_str("</mentioned-files>");
    out
}

/// Same heuristic git uses: a NUL byte anywhere in the first 8000 bytes
/// means "binary" (JARs, images, `.class` files, ...). Cheap and has no
/// false positives on real source text.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Workspace-relative file listing for a mentioned directory, capped so a
/// huge tree can't swamp the prompt.
fn dir_listing(root: &Path) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;

    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if lines.len() >= MAX_DIR_ENTRIES {
            truncated = true;
            break;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            lines.push(rel.to_string_lossy().into_owned());
        }
    }

    lines.sort();
    if truncated {
        lines.push(format!("[truncated at {MAX_DIR_ENTRIES} files]"));
    }
    if lines.is_empty() {
        return "[empty directory]".to_string();
    }
    lines.join("\n")
}

/// Shared by the `@`-completer so suggestions and expansion skip the same
/// noise directories.
pub(crate) fn is_ignored(entry: &walkdir::DirEntry) -> bool {
    matches!(
        entry.file_name().to_str(),
        Some(
            ".git"
                | "target"
                | "node_modules"
                | "dist"
                | "out"
                | "build"
                | ".next"
                | ".venv"
                | "__pycache__"
                | ".DS_Store"
        )
    )
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
    fn expands_a_directory_mention_into_a_listing() {
        let ws = test_workspace("expands_dir");
        fs::create_dir_all(ws.root.join("salesforce/src")).unwrap();
        fs::write(ws.root.join("salesforce/README.md"), "readme").unwrap();
        fs::write(ws.root.join("salesforce/src/app.js"), "code").unwrap();

        let out = expand_mentions("analyse @salesforce", &ws);
        assert!(out.contains("<directory path=\"salesforce\">"));
        assert!(out.contains("README.md"));
        assert!(out.contains("src/app.js"));
    }

    #[test]
    fn directory_mention_accepts_a_trailing_slash() {
        let ws = test_workspace("dir_slash");
        fs::create_dir_all(ws.root.join("pkg")).unwrap();
        fs::write(ws.root.join("pkg/main.rs"), "fn main() {}").unwrap();

        let out = expand_mentions("look at @pkg/", &ws);
        assert!(out.contains("<directory"));
        assert!(out.contains("main.rs"));
    }

    #[test]
    fn directory_listing_skips_ignored_dirs() {
        let ws = test_workspace("dir_ignored");
        fs::create_dir_all(ws.root.join("app/node_modules/left-pad")).unwrap();
        fs::write(ws.root.join("app/index.js"), "code").unwrap();
        fs::write(ws.root.join("app/node_modules/left-pad/i.js"), "dep").unwrap();

        let out = expand_mentions("@app", &ws);
        assert!(out.contains("index.js"));
        assert!(!out.contains("left-pad"));
    }

    #[test]
    fn skips_binary_files_like_a_stray_jar() {
        let ws = test_workspace("binary");
        fs::write(
            ws.root.join("lib.jar"),
            [0x50, 0x4b, 0x03, 0x04, 0x00, 0x00],
        )
        .unwrap();
        let out = expand_mentions("check @lib.jar", &ws);
        assert_eq!(out, "check @lib.jar");
    }

    #[test]
    fn directory_listing_skips_gradle_and_python_build_dirs() {
        let ws = test_workspace("dir_build_dirs");
        fs::create_dir_all(ws.root.join("app/build/classes")).unwrap();
        fs::create_dir_all(ws.root.join("app/__pycache__")).unwrap();
        fs::write(ws.root.join("app/Main.java"), "class Main {}").unwrap();
        fs::write(ws.root.join("app/build/classes/Main.class"), [0u8; 4]).unwrap();
        fs::write(ws.root.join("app/__pycache__/x.pyc"), [0u8; 4]).unwrap();

        let out = expand_mentions("@app", &ws);
        assert!(out.contains("Main.java"));
        assert!(!out.contains("build/classes"));
        assert!(!out.contains("__pycache__"));
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
