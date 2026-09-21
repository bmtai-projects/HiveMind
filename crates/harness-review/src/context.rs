use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{GitRepository, NormalizedDiff, ReviewError, content_hash};

#[derive(Debug, Error)]
pub enum ContextError {
    #[error(transparent)]
    Review(#[from] ReviewError),
    #[error("could not read context file {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("context file {0:?} is not valid UTF-8")]
    NonUtf8(String),
    #[error("context source {0:?} changed after diff acquisition")]
    Stale(String),
    #[error("repository rules exceed the configured limit of {0} bytes")]
    RulesTooLarge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimits {
    pub surrounding_lines: usize,
    pub max_context_bytes: usize,
    pub max_file_bytes: usize,
    pub max_rules_bytes: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            surrounding_lines: 12,
            max_context_bytes: 256 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
            max_rules_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub reason: String,
    pub text: String,
    /// Hash of the complete source file, not just this excerpt.
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundle {
    pub items: Vec<ContextItem>,
    pub total_bytes: usize,
    pub fingerprint: String,
    #[serde(default)]
    pub coverage_notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDocument {
    pub path: String,
    pub content: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryRules {
    pub documents: Vec<RuleDocument>,
    pub total_bytes: usize,
    pub fingerprint: String,
}

/// Build bounded source excerpts around changed hunks. This intentionally
/// does not claim Tree-sitter/LSP semantics: the MVP gives the model exact
/// changed code plus surrounding source and makes that coverage explicit.
pub fn build_context(
    repo: &GitRepository,
    diff: &NormalizedDiff,
    limits: ContextLimits,
) -> Result<ContextBundle, ContextError> {
    let mut items = Vec::new();
    let mut total_bytes = 0usize;
    let mut coverage_notes = Vec::new();

    'files: for file in &diff.files {
        if file.binary {
            coverage_notes.push(format!(
                "{}: binary file; content was not inspected",
                file.path
            ));
            continue;
        }
        if file.content_hash.is_none() {
            coverage_notes.push(format!(
                "{}: deleted file; target content is unavailable",
                file.path
            ));
            continue;
        }
        let Some(bytes) = repo.read_file(diff, &file.path, limits.max_file_bytes)? else {
            return Err(ContextError::Stale(file.path.clone()));
        };
        let actual_hash = content_hash(&bytes);
        if file.content_hash.as_deref() != Some(actual_hash.as_str()) {
            return Err(ContextError::Stale(file.path.clone()));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| ContextError::NonUtf8(file.path.clone()))?;
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }

        let mut ranges: Vec<(usize, usize)> = file
            .hunks
            .iter()
            .map(|hunk| {
                let first = hunk.new_start.max(1) as usize;
                let changed = hunk.new_count.max(1) as usize;
                let start = first.saturating_sub(limits.surrounding_lines + 1);
                let end = first
                    .saturating_add(changed)
                    .saturating_add(limits.surrounding_lines)
                    .saturating_sub(1)
                    .min(lines.len());
                (start, end)
            })
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            match merged.last_mut() {
                Some((_, previous_end)) if start <= previous_end.saturating_add(1) => {
                    *previous_end = (*previous_end).max(end);
                }
                _ => merged.push((start, end)),
            }
        }

        for (start, end) in merged {
            let excerpt = lines[start..end].join("\n");
            if total_bytes.saturating_add(excerpt.len()) > limits.max_context_bytes {
                coverage_notes.push(format!(
                    "context limit reached at {}; later hunks were not included",
                    file.path
                ));
                break 'files;
            }
            total_bytes += excerpt.len();
            items.push(ContextItem {
                path: file.path.clone(),
                start_line: (start + 1) as u32,
                end_line: end as u32,
                reason: "changed hunk with bounded surrounding lines".into(),
                text: excerpt,
                content_hash: actual_hash.clone(),
            });
        }
        if total_bytes >= limits.max_context_bytes {
            break;
        }
    }

    let fingerprint = context_fingerprint(&items);
    Ok(ContextBundle {
        items,
        total_bytes,
        fingerprint,
        coverage_notes,
    })
}

/// Load deterministic repository instructions without following symlinks.
/// Root formats and nested `AGENTS.md` files for changed paths are supported;
/// every document remains untrusted model data and cannot alter tool policy.
pub fn load_repository_rules(
    root: &Path,
    changed_paths: impl IntoIterator<Item = impl AsRef<str>>,
    max_bytes: usize,
) -> Result<RepositoryRules, ContextError> {
    let root = root.canonicalize().map_err(|source| ContextError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut candidates = BTreeSet::from([
        PathBuf::from("AGENTS.md"),
        PathBuf::from("CLAUDE.md"),
        PathBuf::from(".github/copilot-instructions.md"),
    ]);
    for changed in changed_paths {
        let path = Path::new(changed.as_ref());
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            {
                candidates.insert(directory.join("AGENTS.md"));
            }
            parent = directory.parent();
        }
    }
    let rules_dir = root.join(".hypmind").join("rules");
    if rules_dir.is_dir() && !rules_dir.is_symlink() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
            .map_err(|source| ContextError::Io {
                path: rules_dir.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .collect();
        entries.sort();
        for path in entries {
            if let Ok(relative) = path.strip_prefix(&root) {
                candidates.insert(relative.to_path_buf());
            }
        }
    }

    let mut documents = Vec::new();
    let mut total_bytes = 0usize;
    for relative in candidates {
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            continue;
        }
        let absolute = root.join(&relative);
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ContextError::Io {
                    path: absolute,
                    source,
                });
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let next_total = total_bytes.saturating_add(metadata.len() as usize);
        if next_total > max_bytes {
            return Err(ContextError::RulesTooLarge(max_bytes));
        }
        let remaining = max_bytes.saturating_sub(total_bytes);
        let file = std::fs::File::open(&absolute).map_err(|source| ContextError::Io {
            path: absolute.clone(),
            source,
        })?;
        let mut bytes = Vec::with_capacity((metadata.len() as usize).min(remaining));
        file.take(remaining.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| ContextError::Io {
                path: absolute.clone(),
                source,
            })?;
        if bytes.len() > remaining {
            return Err(ContextError::RulesTooLarge(max_bytes));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| ContextError::NonUtf8(relative.display().to_string()))?;
        total_bytes = total_bytes.saturating_add(content.len());
        let relative = relative
            .to_str()
            .ok_or_else(|| ContextError::NonUtf8(relative.display().to_string()))?;
        documents.push(RuleDocument {
            path: relative.replace('\\', "/"),
            content_hash: content_hash(content.as_bytes()),
            content,
        });
    }

    let mut material = Vec::new();
    for document in &documents {
        material.extend_from_slice(document.path.as_bytes());
        material.push(0);
        material.extend_from_slice(document.content_hash.as_bytes());
        material.push(0xff);
    }
    Ok(RepositoryRules {
        documents,
        total_bytes,
        fingerprint: content_hash(material),
    })
}

fn context_fingerprint(items: &[ContextItem]) -> String {
    let mut material = Vec::new();
    for item in items {
        material.extend_from_slice(item.path.as_bytes());
        material.extend_from_slice(&item.start_line.to_le_bytes());
        material.extend_from_slice(&item.end_line.to_le_bytes());
        material.extend_from_slice(item.content_hash.as_bytes());
        material.push(0xff);
    }
    content_hash(material)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiffLimits, ReviewTarget};
    use std::process::Command;
    use tempfile::TempDir;

    fn run(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    fn repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "--quiet"]);
        run(dir.path(), &["config", "user.name", "Test"]);
        run(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        let content = (1..=60)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("code.rs"), format!("{content}\n")).unwrap();
        run(dir.path(), &["add", "code.rs"]);
        run(dir.path(), &["commit", "--quiet", "-m", "base"]);
        dir
    }

    #[test]
    fn context_is_bounded_merged_and_hash_addressed() {
        let dir = repo();
        let path = dir.path().join("code.rs");
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[20] = "changed 21".into();
        lines[25] = "changed 26".into();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        let context = build_context(
            &repo,
            &diff,
            ContextLimits {
                surrounding_lines: 3,
                ..ContextLimits::default()
            },
        )
        .unwrap();
        assert_eq!(
            context.items.len(),
            1,
            "overlapping hunk ranges should merge"
        );
        assert!(context.items[0].text.contains("changed 21"));
        assert!(context.items[0].text.contains("changed 26"));
        assert_eq!(context.items[0].content_hash.len(), 64);
    }

    #[test]
    fn repository_rules_are_ordered_and_include_nested_agents() {
        let dir = repo();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.path().join(".hypmind/rules")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        std::fs::write(dir.path().join("src/AGENTS.md"), "nested").unwrap();
        std::fs::write(dir.path().join(".hypmind/rules/security.md"), "secure").unwrap();
        let rules = load_repository_rules(dir.path(), ["src/nested/code.rs"], 1024).unwrap();
        let paths: Vec<&str> = rules
            .documents
            .iter()
            .map(|rule| rule.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [".hypmind/rules/security.md", "AGENTS.md", "src/AGENTS.md"]
        );
    }

    #[test]
    fn repository_rules_fail_closed_at_the_byte_limit() {
        let dir = repo();
        std::fs::write(dir.path().join("AGENTS.md"), "too many bytes").unwrap();
        let error = load_repository_rules(dir.path(), ["code.rs"], 4).unwrap_err();
        assert!(matches!(error, ContextError::RulesTooLarge(4)));
    }

    #[test]
    fn stale_context_is_rejected() {
        let dir = repo();
        std::fs::write(dir.path().join("code.rs"), "first change\n").unwrap();
        let repo = GitRepository::open(dir.path()).unwrap();
        let diff = repo
            .acquire(ReviewTarget::WorkingTree, DiffLimits::default())
            .unwrap();
        std::fs::write(dir.path().join("code.rs"), "second change\n").unwrap();
        let error = build_context(&repo, &diff, ContextLimits::default()).unwrap_err();
        assert!(matches!(error, ContextError::Stale(_)));
    }
}
