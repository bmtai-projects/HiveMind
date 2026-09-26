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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all_targets() -> Vec<ReviewTarget> {
        vec![
            ReviewTarget::WorkingTree,
            ReviewTarget::Staged,
            ReviewTarget::Commit { sha: "abc".into() },
            ReviewTarget::Range {
                base: "main".into(),
                head: "feature".into(),
            },
            ReviewTarget::PullRequest {
                provider: ScmProvider::GitHub,
                id: "42".into(),
            },
            ReviewTarget::PullRequest {
                provider: ScmProvider::Other("gitea".into()),
                id: "7".into(),
            },
        ]
    }

    #[test]
    fn display_is_human_readable() {
        let shown: Vec<String> = all_targets().iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "working tree",
                "staged changes",
                "commit abc",
                "main..feature",
                "GitHub pull request 42",
                "Other(\"gitea\") pull request 7",
            ]
        );
    }

    /// The serialized form is part of the versioned report schema (and feeds
    /// the target hash), so its exact shape is pinned here.
    #[test]
    fn serialized_shape_is_stable() {
        let values: Vec<_> = all_targets()
            .iter()
            .map(|t| serde_json::to_value(t).unwrap())
            .collect();
        assert_eq!(
            values,
            [
                json!({"kind": "working_tree"}),
                json!({"kind": "staged"}),
                json!({"kind": "commit", "sha": "abc"}),
                json!({"kind": "range", "base": "main", "head": "feature"}),
                json!({"kind": "pull_request", "provider": "git_hub", "id": "42"}),
                json!({"kind": "pull_request", "provider": {"other": "gitea"}, "id": "7"}),
            ]
        );
    }

    #[test]
    fn every_target_round_trips_through_json() {
        for target in all_targets() {
            let text = serde_json::to_string(&target).unwrap();
            let back: ReviewTarget = serde_json::from_str(&text).unwrap();
            assert_eq!(back, target, "{text}");
        }
    }

    #[test]
    fn unknown_kinds_and_missing_fields_are_rejected() {
        let parses = |v| serde_json::from_value::<ReviewTarget>(v).is_ok();
        assert!(!parses(json!({"kind": "stash"})));
        assert!(!parses(json!({"kind": "commit"})));
        assert!(!parses(json!({"kind": "range", "base": "a"})));
    }
}
