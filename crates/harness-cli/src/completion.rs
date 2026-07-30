//! Live completion for the REPL: `/`-prefixed slash commands, and
//! `@`-prefixed file paths fuzzy-matched against the workspace.

use std::path::PathBuf;

use reedline::{Completer, Span, Suggestion};

use crate::commands::COMMAND_NAMES;
use crate::mentions::is_ignored;

pub struct HiveCompleter {
    workdir: PathBuf,
}

impl HiveCompleter {
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    fn file_suggestions(&self, partial: &str, span_start: usize, pos: usize) -> Vec<Suggestion> {
        let mut matches: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&self.workdir)
            .into_iter()
            .filter_entry(|e| !is_ignored(e))
            .filter_map(Result::ok)
        {
            let is_dir = entry.file_type().is_dir();
            if !entry.file_type().is_file() && !is_dir {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.workdir) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue; // the workdir itself
            }
            let rel_str = rel.to_string_lossy();
            if partial.is_empty() || rel_str.contains(partial) {
                // Trailing slash marks a folder mention, which expands to a
                // listing rather than file content.
                matches.push(if is_dir {
                    format!("{rel_str}/")
                } else {
                    rel_str.into_owned()
                });
            }
            if matches.len() >= 50 {
                break;
            }
        }
        matches.sort();

        matches
            .into_iter()
            .map(|path| Suggestion {
                value: format!("@{path}"),
                description: None,
                style: None,
                extra: None,
                span: Span::new(span_start, pos),
                append_whitespace: true,
            })
            .collect()
    }
}

impl Completer for HiveCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let before_cursor = &line[..pos.min(line.len())];

        if let Some(stripped) = before_cursor.strip_prefix('/') {
            if stripped.contains(char::is_whitespace) {
                return Vec::new();
            }
            return COMMAND_NAMES
                .iter()
                .filter(|name| name.starts_with(before_cursor))
                .map(|name| Suggestion {
                    value: name.to_string(),
                    description: None,
                    style: None,
                    extra: None,
                    span: Span::new(0, pos),
                    append_whitespace: true,
                })
                .collect();
        }

        if let Some(at_idx) = find_mention_start(before_cursor) {
            let partial = &before_cursor[at_idx + 1..];
            return self.file_suggestions(partial, at_idx, pos);
        }

        Vec::new()
    }
}

/// Byte index of the `@` that starts the mention currently being typed just
/// before the cursor, if any — the nearest `@` with no whitespace between
/// it and the cursor.
fn find_mention_start(before_cursor: &str) -> Option<usize> {
    let last_at = before_cursor.rfind('@')?;
    let after = &before_cursor[last_at + 1..];
    if after.contains(char::is_whitespace) {
        return None;
    }
    Some(last_at)
}

#[cfg(test)]
mod tests {
    use super::find_mention_start;

    #[test]
    fn finds_mention_at_end_of_line() {
        assert_eq!(find_mention_start("look at @src/ma"), Some(8));
    }

    #[test]
    fn no_mention_without_at_sign() {
        assert_eq!(find_mention_start("look at src/ma"), None);
    }

    #[test]
    fn whitespace_after_at_breaks_the_mention() {
        assert_eq!(find_mention_start("@foo bar"), None);
    }

    #[test]
    fn bare_at_at_cursor_is_a_mention_start() {
        assert_eq!(find_mention_start("hello @"), Some(6));
    }
}
