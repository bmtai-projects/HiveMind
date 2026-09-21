use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::{Args, ValueEnum};
use harness_config::CliOverrides;
use harness_review::{
    ContextLimits, DiffLimits, GitRepository, ReviewEventEnvelope, ReviewFocus, ReviewId,
    ReviewReport, ReviewStore, ReviewSummary, ReviewTarget, UsageSummary, build_context,
    build_review_report, load_repository_rules, render_markdown, render_terminal, stable_hash,
};
use serde::Serialize;

use harness_agent::AgentReviewSampler;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReviewFormat {
    Terminal,
    Json,
    Ndjson,
    Markdown,
}

#[derive(Args, Debug)]
pub(crate) struct ReviewArgs {
    /// Review only changes currently staged in the Git index.
    #[arg(long, conflicts_with_all = ["base", "commit", "range"])]
    staged: bool,

    /// Review HEAD against its merge base with this branch or revision.
    #[arg(long, value_name = "REV", conflicts_with_all = ["staged", "commit", "range"])]
    base: Option<String>,

    /// Review the change introduced by one commit (first-parent for merges).
    #[arg(long, value_name = "REV", conflicts_with_all = ["staged", "base", "range"])]
    commit: Option<String>,

    /// Review a Git range written as BASE..HEAD.
    #[arg(long, value_name = "BASE..HEAD", conflicts_with_all = ["staged", "base", "commit"])]
    range: Option<String>,

    /// Emphasize security defects during candidate generation.
    #[arg(long, conflicts_with_all = ["performance", "tests"])]
    security: bool,

    /// Emphasize performance defects during candidate generation.
    #[arg(long, conflicts_with_all = ["security", "tests"])]
    performance: bool,

    /// Emphasize missing or inadequate tests during candidate generation.
    #[arg(long, conflicts_with_all = ["security", "performance"])]
    tests: bool,

    /// Report representation. NDJSON emits versioned progress events.
    #[arg(long, value_enum, default_value_t = ReviewFormat::Terminal)]
    format: ReviewFormat,

    /// Git repository root to review.
    #[arg(long, default_value = ".")]
    workdir: PathBuf,

    /// Path to config.toml. Defaults to ~/.config/hivemind/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Model used for candidate generation and evidence validation.
    #[arg(long)]
    model: Option<String>,

    /// Override the HiveMind API key.
    #[arg(long)]
    api_key: Option<String>,

    /// Override the model API base URL.
    #[arg(long)]
    base_url: Option<String>,

    /// Reasoning effort for models that support it.
    #[arg(long)]
    reasoning_effort: Option<String>,

    /// Maximum estimated model spend for each bounded review pass.
    #[arg(long)]
    budget: Option<f64>,

    /// Maximum number of changed files accepted in one review.
    #[arg(long, default_value_t = 100, value_parser = parse_positive_usize)]
    max_files: usize,

    /// Maximum normalized patch bytes accepted in one review.
    #[arg(long, default_value_t = 512 * 1024, value_parser = parse_positive_usize)]
    max_diff_bytes: usize,

    /// Maximum source bytes loaded from any changed file.
    #[arg(long, default_value_t = 2 * 1024 * 1024, value_parser = parse_positive_usize)]
    max_file_bytes: usize,

    /// Maximum total bytes of source excerpts sent to the reviewer.
    #[arg(long, default_value_t = 256 * 1024, value_parser = parse_positive_usize)]
    max_context_bytes: usize,

    /// Source lines retained on each side of a changed hunk.
    #[arg(long, default_value_t = 12)]
    context_lines: usize,
}

pub(crate) async fn run(args: ReviewArgs) -> anyhow::Result<()> {
    let review_id = new_review_id();
    let mut events = EventWriter::new(args.format, review_id.clone());
    events.emit(
        "review.started",
        serde_json::json!({
            "workdir": args.workdir.display().to_string(),
        }),
    )?;

    let result = run_inner(&args, review_id, &mut events).await;
    match result {
        Ok(report) => {
            for finding in &report.findings {
                events.emit("review.finding", finding)?;
            }
            events.emit("review.completed", &report)?;
            write_report(args.format, &report)?;
            Ok(())
        }
        Err(error) => {
            events.emit(
                "review.failed",
                serde_json::json!({ "message": format!("{error:#}") }),
            )?;
            Err(error)
        }
    }
}

async fn run_inner(
    args: &ReviewArgs,
    review_id: ReviewId,
    events: &mut EventWriter,
) -> anyhow::Result<ReviewReport> {
    let workdir = args
        .workdir
        .canonicalize()
        .with_context(|| format!("workdir {:?}", args.workdir))?;
    let workspace = workdir
        .to_str()
        .context("the repository root is not valid UTF-8")?
        .replace('\\', "/");
    let workspace_fingerprint = stable_hash(workspace.as_bytes());
    let repository = GitRepository::open(&workdir)?;
    let target = requested_target(args, &repository)?;
    let diff = repository.acquire(
        target,
        DiffLimits {
            max_files: args.max_files,
            max_bytes: args.max_diff_bytes,
            max_file_bytes: args.max_file_bytes,
        },
    )?;
    events.emit(
        "review.target_collected",
        serde_json::json!({
            "target": diff.target,
            "base_revision": diff.base_revision,
            "head_revision": diff.head_revision,
            "diff_fingerprint": diff.fingerprint,
            "stats": diff.stats,
        }),
    )?;

    let context_limits = ContextLimits {
        surrounding_lines: args.context_lines,
        max_context_bytes: args.max_context_bytes,
        max_file_bytes: args.max_file_bytes,
        ..ContextLimits::default()
    };
    let context = build_context(&repository, &diff, context_limits)?;
    let rules = load_repository_rules(
        &workdir,
        diff.files.iter().map(|file| file.path.as_str()),
        context_limits.max_rules_bytes,
    )?;
    events.emit(
        "review.context_built",
        serde_json::json!({
            "items": context.items.len(),
            "bytes": context.total_bytes,
            "fingerprint": context.fingerprint,
            "rule_documents": rules.documents.len(),
            "coverage_notes": context.coverage_notes,
        }),
    )?;

    let created_at = unix_now();
    let report = if diff.files.is_empty() {
        empty_report(
            review_id,
            &diff,
            workspace_fingerprint,
            context.coverage_notes,
            created_at,
        )
    } else {
        eprintln!("review: generating and independently validating evidence-backed candidates...");
        let config_path = args
            .config
            .clone()
            .unwrap_or_else(harness_config::default_config_path);
        let resolved = harness_config::resolve(
            &config_path,
            &harness_config::default_credentials_path(),
            CliOverrides {
                api_key: args.api_key.clone(),
                base_url: args.base_url.clone(),
                model: args.model.clone(),
                reasoning_effort: args.reasoning_effort.clone(),
                budget_usd: args.budget,
                mode: None,
            },
        )?;
        let sampler = AgentReviewSampler::new(resolved, workdir.clone());
        build_review_report(
            &sampler,
            &diff,
            &context,
            &rules,
            review_focus(args),
            review_id,
            workspace_fingerprint,
            created_at,
        )
        .await?
    };

    ReviewStore::new(harness_config::default_reviews_dir())
        .save(&workdir, &report)
        .context("persist review report")?;
    Ok(report)
}

fn review_focus(args: &ReviewArgs) -> ReviewFocus {
    if args.security {
        ReviewFocus::Security
    } else if args.performance {
        ReviewFocus::Performance
    } else if args.tests {
        ReviewFocus::Tests
    } else {
        ReviewFocus::All
    }
}

fn requested_target(args: &ReviewArgs, repository: &GitRepository) -> anyhow::Result<ReviewTarget> {
    if args.staged {
        return Ok(ReviewTarget::Staged);
    }
    if let Some(base) = &args.base {
        let base = repository.merge_base(base, "HEAD")?;
        return Ok(ReviewTarget::Range {
            base,
            head: "HEAD".into(),
        });
    }
    if let Some(commit) = &args.commit {
        return Ok(ReviewTarget::Commit {
            sha: commit.clone(),
        });
    }
    if let Some(range) = &args.range {
        let (base, head) = parse_range(range)?;
        return Ok(ReviewTarget::Range { base, head });
    }
    Ok(ReviewTarget::WorkingTree)
}

fn parse_range(value: &str) -> anyhow::Result<(String, String)> {
    if value.contains("...") || value.matches("..").count() != 1 {
        anyhow::bail!("--range must use exactly BASE..HEAD (three-dot ranges are not accepted)");
    }
    let (base, head) = value
        .split_once("..")
        .context("--range must use BASE..HEAD")?;
    if base.is_empty() || head.is_empty() {
        anyhow::bail!("--range must include both BASE and HEAD");
    }
    Ok((base.into(), head.into()))
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{value:?} is not a positive integer"))?;
    if parsed == 0 {
        return Err("value must be greater than zero".into());
    }
    Ok(parsed)
}

fn empty_report(
    review_id: ReviewId,
    diff: &harness_review::NormalizedDiff,
    workspace_fingerprint: String,
    coverage_notes: Vec<String>,
    created_at: u64,
) -> ReviewReport {
    let mut summary = ReviewSummary {
        files_reviewed: 0,
        hunks_reviewed: 0,
        coverage_notes,
        ..ReviewSummary::default()
    };
    summary.count_findings(&[]);
    ReviewReport::new(
        review_id,
        diff.target.clone(),
        diff.base_revision.clone(),
        diff.head_revision.clone(),
        diff.fingerprint.clone(),
        workspace_fingerprint,
        summary,
        Vec::new(),
        UsageSummary::default(),
        created_at,
    )
}

fn write_report(format: ReviewFormat, report: &ReviewReport) -> anyhow::Result<()> {
    match format {
        ReviewFormat::Terminal => print!("{}", render_terminal(report)),
        ReviewFormat::Markdown => print!("{}", render_markdown(report)),
        ReviewFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        ReviewFormat::Ndjson => {}
    }
    std::io::stdout().flush()?;
    Ok(())
}

struct EventWriter {
    enabled: bool,
    review_id: ReviewId,
    sequence: u64,
}

impl EventWriter {
    fn new(format: ReviewFormat, review_id: ReviewId) -> Self {
        Self {
            enabled: format == ReviewFormat::Ndjson,
            review_id,
            sequence: 0,
        }
    }

    fn emit(&mut self, event: &str, data: impl Serialize) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.sequence += 1;
        let envelope = ReviewEventEnvelope::new(
            self.review_id.clone(),
            self.sequence,
            event,
            serde_json::to_value(data)?,
        );
        println!("{}", serde_json::to_string(&envelope)?);
        std::io::stdout().flush()?;
        Ok(())
    }
}

fn new_review_id() -> ReviewId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let entropy = format!("{now}:{}", std::process::id());
    ReviewId(format!(
        "review_{now}_{:.12}",
        stable_hash(entropy.as_bytes())
    ))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct ReviewCli {
        #[command(flatten)]
        review: ReviewArgs,
    }

    fn parse(args: &[&str]) -> ReviewArgs {
        ReviewCli::try_parse_from(std::iter::once("review-test").chain(args.iter().copied()))
            .unwrap()
            .review
    }

    #[test]
    fn targets_are_mutually_exclusive_at_the_cli_boundary() {
        let result = ReviewCli::try_parse_from(["review-test", "--staged", "--base", "main"]);
        assert!(result.is_err());
    }

    #[test]
    fn focuses_are_mutually_exclusive_at_the_cli_boundary() {
        let result = ReviewCli::try_parse_from(["review-test", "--security", "--performance"]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_only_two_dot_ranges() {
        assert_eq!(
            parse_range("main..feature").unwrap(),
            ("main".into(), "feature".into())
        );
        assert!(parse_range("main...feature").is_err());
        assert!(parse_range("..feature").is_err());
        assert!(parse_range("main..").is_err());
    }

    #[test]
    fn format_defaults_to_terminal() {
        assert_eq!(parse(&[]).format, ReviewFormat::Terminal);
        assert_eq!(parse(&["--format", "ndjson"]).format, ReviewFormat::Ndjson);
    }

    #[test]
    fn zero_safety_limits_are_rejected() {
        assert!(ReviewCli::try_parse_from(["review-test", "--max-files", "0"]).is_err());
    }

    #[test]
    fn review_ids_are_transport_safe_and_unique() {
        let first = new_review_id();
        let second = new_review_id();
        assert!(first.0.starts_with("review_"));
        assert_ne!(first, second);
        assert!(
            first
                .0
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
    }

    #[test]
    fn empty_report_preserves_target_fingerprint() {
        let diff = harness_review::NormalizedDiff {
            target: ReviewTarget::WorkingTree,
            base_revision: Some("base".into()),
            head_revision: None,
            fingerprint: "diff".into(),
            files: Vec::new(),
            stats: Default::default(),
        };
        let report = empty_report(
            ReviewId("review_test".into()),
            &diff,
            "workspace".into(),
            Vec::new(),
            42,
        );
        assert_eq!(report.diff_fingerprint, "diff");
        assert!(report.findings.is_empty());
    }

    #[test]
    fn rule_bundle_type_remains_serializable_for_the_sampler_boundary() {
        let rules = harness_review::RepositoryRules::default();
        assert_eq!(
            serde_json::to_string(&rules).unwrap(),
            r#"{"documents":[],"total_bytes":0,"fingerprint":""}"#
        );
    }
}
