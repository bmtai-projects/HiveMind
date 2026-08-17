use serde::{Deserialize, Serialize};

/// Source-control provider for a future remote pull-request target. The
/// local Git implementation rejects this target explicitly; keeping it in
/// the versioned schema now prevents a later transport-specific fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScmProvider {
    GitHub,
    GitLab,
    Bitbucket,
    Other(String),
}

/// One review source, normalized independently of the CLI flags that
/// selected it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// Staged and unstaged changes against `HEAD`, plus untracked files.
    WorkingTree,
    /// Only changes currently present in the Git index.
    Staged,
    /// The change introduced by one commit.
    Commit { sha: String },
    /// Changes reachable between two revisions (`base..head`).
    Range { base: String, head: String },
    /// Reserved for the later hosted integration.
    PullRequest { provider: ScmProvider, id: String },
}

impl std::fmt::Display for ReviewTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkingTree => f.write_str("working tree"),
            Self::Staged => f.write_str("staged changes"),
            Self::Commit { sha } => write!(f, "commit {sha}"),
            Self::Range { base, head } => write!(f, "{base}..{head}"),
            Self::PullRequest { provider, id } => write!(f, "{provider:?} pull request {id}"),
        }
    }
}
