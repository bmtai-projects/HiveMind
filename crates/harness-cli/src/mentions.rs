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
            resolved.push((
                candidate.to_string(),
                Mentioned::Dir(dir_listing(&path, workspace)),
            ));
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
///
/// The paths are relative to the **workspace**, not to the mentioned
/// directory, and that distinction is the whole point. Every file tool
/// resolves its `path` argument against the workspace root, so a listing
/// relative to the mention is a list of paths that are guaranteed to fail
/// the moment the model uses one.
///
/// That is not hypothetical. With a workspace open on a directory holding
/// several repositories -- the ordinary way to work on more than one --
/// `@HiveMind/` produced `crates/harness-cli/src/main.rs`, while the only
/// path any tool would accept was `HiveMind/crates/harness-cli/src/main.rs`.
/// The model used what it was given, got "No such file or directory" from
/// every call, and spent turns probing with `ls` and `echo $PWD` before
/// working out on its own that the tools resolve against the workspace root.
/// It was reasoning correctly from information the harness had made up.
fn dir_listing(root: &Path, workspace: &Workspace) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;

    // `Workspace` stores the root exactly as it was given but canonicalizes
    // inside `resolve`, so `root` here is already canonical while
    // `workspace.root` may not be -- on macOS that is `/private/var/...`
    // against `/var/...`, and stripping one from the other silently yields
    // nothing. Canonicalize the same way `resolve` does, with the same
    // fallback, so the two agree by construction.
    let workspace_root = workspace
        .root
        .canonicalize()
        .unwrap_or_else(|_| workspace.root.clone());

    // The same walk the read-only tools use, so a mention shows exactly the
    // files those tools would find. This listing used to keep its own ignore
    // list, which is how mentioning a JS project pasted in its `.next/`
    // build output until it hit the cap below.
    for path in harness_tools::walk_files(root) {
        if lines.len() >= MAX_DIR_ENTRIES {
            truncated = true;
            break;
        }
        if let Ok(rel) = path.strip_prefix(&workspace_root) {
            // Normalize to forward slashes so the listing is the same text
            // on every platform, not just whichever one is rendering it.
            lines.push(rel.to_string_lossy().replace('\\', "/"));
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
        let dir = std::env::temp_dir().join(format!(
            "hivemind_mentions_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
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

    /// The regression that made `@repo/` unusable from a workspace holding
    /// more than one repository. Every path a mention lists has to be one a
    /// file tool will actually accept -- i.e. resolvable by `Workspace` --
    /// and the previous listing was relative to the mentioned directory, so
    /// none of them were.
    ///
    /// The pre-existing test above did not catch this: it asserted the
    /// output *contained* `src/app.js`, which is equally true of the correct
    /// `salesforce/src/app.js` and the broken `src/app.js`. This asserts the
    /// property that actually matters instead of a substring.
    #[test]
    fn every_path_a_directory_mention_lists_is_one_the_tools_can_resolve() {
        let ws = test_workspace("multi_repo");
        fs::create_dir_all(ws.root.join("HiveMind/crates/harness-cli/src")).unwrap();
        fs::create_dir_all(ws.root.join("HiveMind-site/src")).unwrap();
        fs::write(
            ws.root.join("HiveMind/crates/harness-cli/src/main.rs"),
            "fn main() {}",
        )
        .unwrap();
        fs::write(ws.root.join("HiveMind/Cargo.toml"), "[package]").unwrap();
        // A sibling repo that must not leak into the mentioned one's listing.
        fs::write(ws.root.join("HiveMind-site/src/page.tsx"), "export {}").unwrap();

        let out = expand_mentions("work in @HiveMind/", &ws);
        let listing = out
            .split("<directory path=\"HiveMind/\">\n")
            .nth(1)
            .and_then(|s| s.split("\n</directory>").next())
            .expect("a directory block");

        assert!(!listing.is_empty(), "listing should not be empty");
        for line in listing.lines() {
            assert!(
                line.starts_with("HiveMind/"),
                "listed path {line:?} is not workspace-relative -- no tool can resolve it"
            );
            ws.resolve(line)
                .unwrap_or_else(|e| panic!("listed path {line:?} does not resolve: {e}"));
        }
        assert!(listing.contains("HiveMind/crates/harness-cli/src/main.rs"));
        assert!(
            !listing.contains("page.tsx"),
            "a mention must not list a sibling directory's files"
        );
    }

    /// A mention of a JS project used to paste in its whole build output,
    /// because this listing kept its own ignore list separate from the one
    /// the read-only tools used. It now shares theirs, so `.next/` is
    /// excluded here for the same reason it is excluded from `project_map`.
    #[test]
    fn a_directory_mention_excludes_generated_output() {
        let ws = test_workspace("mention_generated");
        fs::create_dir_all(ws.root.join("site/.next/static")).unwrap();
        fs::create_dir_all(ws.root.join("site/src")).unwrap();
        fs::write(ws.root.join("site/src/page.tsx"), "export {}").unwrap();
        fs::write(ws.root.join("site/.next/static/chunk.js"), "generated").unwrap();

        let out = expand_mentions("@site", &ws);
        assert!(out.contains("site/src/page.tsx"));
        assert!(
            !out.contains("chunk.js"),
            "build output must not reach the prompt"
        );
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
