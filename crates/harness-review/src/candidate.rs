use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::REVIEW_SCHEMA_VERSION;
use crate::context::{ContextBundle, RepositoryRules};
use crate::diff::{DiffHunk, NormalizedDiff, ReviewFile};
use crate::hash::stable_hash;
use crate::report::{
    CodeLocation, EvidenceItem, FindingCategory, FindingId, FindingStatus, PatchStrategy,
    ReviewFinding, ReviewId, ReviewReport, ReviewSummary, Severity, UsageSummary, ValidationStep,
};

const MAX_CANDIDATES: usize = 200;
const MAX_TITLE_BYTES: usize = 240;
const MAX_DETAIL_BYTES: usize = 8 * 1024;
const MAX_EVIDENCE_DESCRIPTION_BYTES: usize = 2 * 1024;

/// Optional category focus selected by the user for this review.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFocus {
    #[default]
    All,
    Security,
    Performance,
    Tests,
}

impl ReviewFocus {
    fn accepts(self, category: FindingCategory) -> bool {
        match self {
            Self::All => true,
            Self::Security => category == FindingCategory::Security,
            Self::Performance => category == FindingCategory::Performance,
            Self::Tests => category == FindingCategory::TestGap,
        }
    }
}

/// Evidence proposed by the candidate-generation model. It is deliberately
/// separate from [`EvidenceItem`]: none of these fields are trusted until
/// they have been matched against the immutable context bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvidence {
    pub location: CodeLocation,
    pub description: String,
    pub content_hash: String,
}

/// One untrusted model-generated finding candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCandidate {
    /// The generator may leave this empty. The harness replaces it with a
    /// deterministic identity before sending the independent validation
    /// request, so model-chosen or duplicate identifiers never reach a
    /// persisted report.
    #[serde(default)]
    pub candidate_id: String,
    pub title: String,
    pub category: FindingCategory,
    pub severity: Severity,
    pub confidence: f32,
    pub primary_location: CodeLocation,
    pub evidence: Vec<CandidateEvidence>,
    pub failure_scenario: String,
    #[serde(default)]
    pub assumptions: Vec<String>,
    pub suggested_action: String,
    #[serde(default)]
    pub validation_plan: Vec<ValidationStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_strategy: Option<PatchStrategy>,
}

/// Complete bounded input for candidate generation. Repository source and
/// rules remain data in this structure; a sampler must not reinterpret them
/// as authorization to call tools or change policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateRequest {
    pub schema_version: String,
    pub diff: NormalizedDiff,
    pub context: ContextBundle,
    pub rules: RepositoryRules,
    pub focus: ReviewFocus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateSubmission {
    #[serde(default)]
    pub candidates: Vec<ReviewCandidate>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SampledCandidates {
    pub submission: CandidateSubmission,
    pub usage: UsageSummary,
}

/// Complete evidence supplied to the independent validator. Supplying only
/// fingerprints here would make the pass independent in name only: it must
/// see the actual diff and source excerpts to challenge a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationRequest {
    pub schema_version: String,
    pub diff: NormalizedDiff,
    pub context: ContextBundle,
    pub rules: RepositoryRules,
    pub focus: ReviewFocus,
    pub candidates: Vec<ReviewCandidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationVerdict {
    pub candidate_id: String,
    pub accepted: bool,
    /// A validator may lower confidence but can never raise the generator's
    /// confidence; that rule is enforced when verdicts are applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationSubmission {
    #[serde(default)]
    pub verdicts: Vec<ValidationVerdict>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SampledValidation {
    pub submission: ValidationSubmission,
    pub usage: UsageSummary,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReviewEngineError {
    #[error("candidate sampling failed: {0}")]
    CandidateSampling(String),
    #[error("validation sampling failed: {0}")]
    ValidationSampling(String),
}

/// Provider-neutral model boundary. Provider JSON extraction, retries, and
/// transport errors belong in the adapter; deterministic evidence checks,
/// verdict application, ranking, and report construction stay here.
// async_trait's expansion already returns a must_use boxed future; newer
// clippy flags the macro's own must_use as redundant. Nothing here to fix.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait ReviewSampler: Send + Sync {
    async fn sample_candidates(
        &self,
        request: &CandidateRequest,
    ) -> Result<SampledCandidates, ReviewEngineError>;

    async fn sample_validation(
        &self,
        request: &ValidationRequest,
    ) -> Result<SampledValidation, ReviewEngineError>;
}

struct ValidCandidate {
    candidate: ReviewCandidate,
    source_hash: String,
    identity: Vec<u8>,
}

#[derive(Default)]
struct PrevalidationStats {
    rejected: usize,
    duplicates: usize,
    capped: usize,
}

/// Generate, independently validate, rank, and aggregate one review report.
/// Zero candidates is a successful clean review and deliberately skips the
/// second model call.
#[allow(clippy::too_many_arguments)]
pub async fn build_review_report<S: ReviewSampler + ?Sized>(
    sampler: &S,
    diff: &NormalizedDiff,
    context: &ContextBundle,
    rules: &RepositoryRules,
    focus: ReviewFocus,
    review_id: ReviewId,
    workspace_fingerprint: String,
    created_at: u64,
) -> Result<ReviewReport, ReviewEngineError> {
    let candidate_request = CandidateRequest {
        schema_version: REVIEW_SCHEMA_VERSION.to_string(),
        diff: diff.clone(),
        context: context.clone(),
        rules: rules.clone(),
        focus,
    };
    let SampledCandidates {
        submission,
        usage: candidate_usage,
    } = sampler.sample_candidates(&candidate_request).await?;

    let (candidates, prevalidation) =
        prevalidate_and_deduplicate(submission.candidates, diff, context, focus);
    let mut coverage_notes = context.coverage_notes.clone();
    if prevalidation.capped > 0 {
        coverage_notes.push(format!(
            "{} model candidate(s) were not considered because the {}-candidate safety limit was reached",
            prevalidation.capped, MAX_CANDIDATES
        ));
    }
    if prevalidation.rejected > 0 {
        coverage_notes.push(format!(
            "{} candidate(s) were rejected by deterministic evidence validation",
            prevalidation.rejected
        ));
    }
    if prevalidation.duplicates > 0 {
        coverage_notes.push(format!(
            "{} duplicate candidate(s) were merged before independent validation",
            prevalidation.duplicates
        ));
    }

    let (mut findings, usage) = if candidates.is_empty() {
        (Vec::new(), candidate_usage)
    } else {
        let validation_request = ValidationRequest {
            schema_version: REVIEW_SCHEMA_VERSION.to_string(),
            diff: diff.clone(),
            context: context.clone(),
            rules: rules.clone(),
            focus,
            candidates: candidates
                .iter()
                .map(|candidate| candidate.candidate.clone())
                .collect(),
        };
        let SampledValidation {
            submission,
            usage: validation_usage,
        } = sampler.sample_validation(&validation_request).await?;
        let (findings, rejected) = apply_verdicts(candidates, submission);
        if rejected > 0 {
            coverage_notes.push(format!(
                "{rejected} candidate(s) were rejected or left unconfirmed by independent validation"
            ));
        }
        (
            findings,
            aggregate_usage(candidate_usage, Some(validation_usage)),
        )
    };

    sort_findings(&mut findings);
    let mut summary = review_summary(diff, context, coverage_notes);
    summary.count_findings(&findings);

    Ok(ReviewReport::new(
        review_id,
        diff.target.clone(),
        diff.base_revision.clone(),
        diff.head_revision.clone(),
        diff.fingerprint.clone(),
        workspace_fingerprint,
        summary,
        findings,
        usage,
        created_at,
    ))
}

fn prevalidate_and_deduplicate(
    candidates: Vec<ReviewCandidate>,
    diff: &NormalizedDiff,
    context: &ContextBundle,
    focus: ReviewFocus,
) -> (Vec<ValidCandidate>, PrevalidationStats) {
    let submitted = candidates.len();
    let mut stats = PrevalidationStats {
        capped: submitted.saturating_sub(MAX_CANDIDATES),
        ..PrevalidationStats::default()
    };
    let mut by_identity: BTreeMap<Vec<u8>, ValidCandidate> = BTreeMap::new();

    for mut candidate in candidates.into_iter().take(MAX_CANDIDATES) {
        let Some((source_hash, identity)) = validate_candidate(&candidate, diff, context, focus)
        else {
            stats.rejected += 1;
            continue;
        };
        candidate.candidate_id = format!("candidate_{}", stable_hash(&identity));
        let candidate = ValidCandidate {
            candidate,
            source_hash,
            identity: identity.clone(),
        };
        match by_identity.entry(identity) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                stats.duplicates += 1;
                if candidate_preference(&candidate.candidate, &entry.get().candidate)
                    == Ordering::Greater
                {
                    entry.insert(candidate);
                }
            }
        }
    }

    (by_identity.into_values().collect(), stats)
}

fn validate_candidate(
    candidate: &ReviewCandidate,
    diff: &NormalizedDiff,
    context: &ContextBundle,
    focus: ReviewFocus,
) -> Option<(String, Vec<u8>)> {
    if !focus.accepts(candidate.category)
        || !candidate.confidence.is_finite()
        || !(0.0..=1.0).contains(&candidate.confidence)
        || !bounded_nonempty(&candidate.title, MAX_TITLE_BYTES)
        || !bounded_nonempty(&candidate.failure_scenario, MAX_DETAIL_BYTES)
        || !bounded_nonempty(&candidate.suggested_action, MAX_DETAIL_BYTES)
        || candidate.evidence.is_empty()
        || !valid_location(&candidate.primary_location)
    {
        return None;
    }

    let file = diff
        .files
        .iter()
        .find(|file| file.path == candidate.primary_location.path)?;
    if file.binary {
        return None;
    }
    let source_hash = file.content_hash.clone()?;
    if !intersects_changed_line(file, &candidate.primary_location) {
        return None;
    }
    if !location_is_in_context(context, &candidate.primary_location, &source_hash) {
        return None;
    }

    for evidence in &candidate.evidence {
        if !bounded_nonempty(&evidence.description, MAX_EVIDENCE_DESCRIPTION_BYTES)
            || !valid_location(&evidence.location)
            || !location_is_in_context(context, &evidence.location, &evidence.content_hash)
        {
            return None;
        }
    }

    Some((source_hash, candidate_identity(diff, candidate)))
}

fn bounded_nonempty(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes
}

fn valid_location(location: &CodeLocation) -> bool {
    location.line > 0
        && location.column != Some(0)
        && location
            .end_line
            .is_none_or(|end_line| end_line >= location.line)
}

fn intersects_changed_line(file: &ReviewFile, location: &CodeLocation) -> bool {
    let changed = file.changed_new_lines();
    let end = location.end_line.unwrap_or(location.line);
    changed.range(location.line..=end).next().is_some()
}

fn location_is_in_context(
    context: &ContextBundle,
    location: &CodeLocation,
    expected_hash: &str,
) -> bool {
    let end = location.end_line.unwrap_or(location.line);
    context.items.iter().any(|item| {
        item.path == location.path
            && item.content_hash == expected_hash
            && location.line >= item.start_line
            && end <= item.end_line
    })
}

fn candidate_identity(diff: &NormalizedDiff, candidate: &ReviewCandidate) -> Vec<u8> {
    let mut identity = Vec::new();
    push_part(&mut identity, diff.fingerprint.as_bytes());
    push_part(&mut identity, category_name(candidate.category).as_bytes());
    push_location(&mut identity, &candidate.primary_location);

    let mut evidence: Vec<&CandidateEvidence> = candidate.evidence.iter().collect();
    evidence.sort_by(|left, right| {
        location_order(&left.location, &right.location)
            .then_with(|| left.content_hash.cmp(&right.content_hash))
    });
    for item in evidence {
        push_location(&mut identity, &item.location);
        push_part(&mut identity, item.content_hash.as_bytes());
    }
    identity
}

fn category_name(category: FindingCategory) -> &'static str {
    match category {
        FindingCategory::Correctness => "correctness",
        FindingCategory::Security => "security",
        FindingCategory::Performance => "performance",
        FindingCategory::Maintainability => "maintainability",
        FindingCategory::TestGap => "test_gap",
    }
}

fn push_location(material: &mut Vec<u8>, location: &CodeLocation) {
    push_part(material, location.path.as_bytes());
    push_part(material, &location.line.to_le_bytes());
    push_part(material, &location.column.unwrap_or(0).to_le_bytes());
    push_part(
        material,
        &location.end_line.unwrap_or(location.line).to_le_bytes(),
    );
}

fn push_part(material: &mut Vec<u8>, part: &[u8]) {
    material.extend_from_slice(&(part.len() as u64).to_le_bytes());
    material.extend_from_slice(part);
}

fn candidate_preference(left: &ReviewCandidate, right: &ReviewCandidate) -> Ordering {
    left.severity
        .cmp(&right.severity)
        .then_with(|| left.confidence.total_cmp(&right.confidence))
        .then_with(|| left.evidence.len().cmp(&right.evidence.len()))
        // Lexically smaller prose wins the final tie, making selection
        // independent of the order in which the model returned duplicates.
        .then_with(|| right.title.cmp(&left.title))
        .then_with(|| right.failure_scenario.cmp(&left.failure_scenario))
}

fn apply_verdicts(
    candidates: Vec<ValidCandidate>,
    submission: ValidationSubmission,
) -> (Vec<ReviewFinding>, usize) {
    let known: BTreeSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.candidate.candidate_id.as_str())
        .collect();
    let mut verdicts: BTreeMap<String, Option<ValidationVerdict>> = BTreeMap::new();
    for verdict in submission.verdicts {
        if !known.contains(verdict.candidate_id.as_str()) {
            continue;
        }
        match verdicts.entry(verdict.candidate_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(verdict));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                // Duplicate verdicts are ambiguous and therefore fail this
                // candidate closed, without discarding unrelated findings.
                entry.insert(None);
            }
        }
    }

    let mut findings = Vec::new();
    let mut rejected = 0usize;
    for candidate in candidates {
        let verdict = verdicts.remove(&candidate.candidate.candidate_id).flatten();
        let Some(verdict) = verdict else {
            rejected += 1;
            continue;
        };
        if !verdict.accepted || verdict.rationale.trim().is_empty() {
            rejected += 1;
            continue;
        }
        let validated_confidence = verdict.confidence.unwrap_or(candidate.candidate.confidence);
        if !validated_confidence.is_finite() || !(0.0..=1.0).contains(&validated_confidence) {
            rejected += 1;
            continue;
        }
        let confidence = candidate.candidate.confidence.min(validated_confidence);
        findings.push(to_finding(candidate, confidence));
    }
    (findings, rejected)
}

fn to_finding(candidate: ValidCandidate, confidence: f32) -> ReviewFinding {
    let finding_id = FindingId(format!("HM-{}", stable_hash(&candidate.identity)));
    let evidence = candidate
        .candidate
        .evidence
        .iter()
        .map(|evidence| EvidenceItem {
            location: evidence.location.clone(),
            description: evidence.description.clone(),
            content_hash: evidence.content_hash.clone(),
        })
        .collect();
    let priority_score = priority_score(&candidate.candidate, confidence);
    ReviewFinding {
        id: finding_id,
        title: candidate.candidate.title,
        category: candidate.candidate.category,
        severity: candidate.candidate.severity,
        confidence,
        priority_score,
        primary_location: candidate.candidate.primary_location,
        source_hash: candidate.source_hash,
        evidence,
        failure_scenario: candidate.candidate.failure_scenario,
        assumptions: candidate.candidate.assumptions,
        suggested_action: candidate.candidate.suggested_action,
        validation_plan: candidate.candidate.validation_plan,
        patch_strategy: candidate.candidate.patch_strategy,
        status: FindingStatus::Open,
    }
}

fn priority_score(candidate: &ReviewCandidate, confidence: f32) -> f32 {
    let evidence_quality = (0.6 + candidate.evidence.len().min(4) as f32 * 0.1).min(1.0);
    let reproducibility = if candidate.validation_plan.is_empty() {
        0.85
    } else {
        1.0
    };
    candidate.severity.weight() * confidence * evidence_quality * reproducibility
}

fn sort_findings(findings: &mut [ReviewFinding]) {
    findings.sort_by(|left, right| {
        right
            .priority_score
            .total_cmp(&left.priority_score)
            .then_with(|| right.severity.cmp(&left.severity))
            .then_with(|| right.confidence.total_cmp(&left.confidence))
            .then_with(|| location_order(&left.primary_location, &right.primary_location))
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
}

fn location_order(left: &CodeLocation, right: &CodeLocation) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.column.cmp(&right.column))
        .then_with(|| left.end_line.cmp(&right.end_line))
}

fn aggregate_usage(first: UsageSummary, second: Option<UsageSummary>) -> UsageSummary {
    let Some(second) = second else {
        return first;
    };
    let model = match (first.model.as_str(), second.model.as_str()) {
        ("", model) => model.to_string(),
        (model, "") => model.to_string(),
        (left, right) if left == right => left.to_string(),
        (left, right) => format!("{left} + {right}"),
    };
    UsageSummary {
        model,
        prompt_tokens: first.prompt_tokens.saturating_add(second.prompt_tokens),
        completion_tokens: first
            .completion_tokens
            .saturating_add(second.completion_tokens),
        total_tokens: first.total_tokens.saturating_add(second.total_tokens),
        estimated_cost_usd: match (first.estimated_cost_usd, second.estimated_cost_usd) {
            (Some(left), Some(right)) => Some(left + right),
            _ => None,
        },
    }
}

fn review_summary(
    diff: &NormalizedDiff,
    context: &ContextBundle,
    coverage_notes: Vec<String>,
) -> ReviewSummary {
    let reviewed_paths: BTreeSet<&str> = context
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    let hunks_reviewed = diff
        .files
        .iter()
        .flat_map(|file| {
            file.hunks
                .iter()
                .map(move |hunk| (file.path.as_str(), hunk))
        })
        .filter(|(path, hunk)| hunk_has_context(path, hunk, context))
        .count();
    ReviewSummary {
        files_reviewed: reviewed_paths.len(),
        hunks_reviewed,
        coverage_notes,
        ..ReviewSummary::default()
    }
}

fn hunk_has_context(path: &str, hunk: &DiffHunk, context: &ContextBundle) -> bool {
    if hunk.new_count == 0 {
        return false;
    }
    let hunk_start = hunk.new_start.max(1);
    let hunk_end = hunk_start.saturating_add(hunk.new_count.saturating_sub(1));
    context
        .items
        .iter()
        .any(|item| item.path == path && item.start_line <= hunk_end && item.end_line >= hunk_start)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::diff::{ChangeKind, DiffLine, DiffLineKind, DiffStats, ReviewFile};
    use crate::hash::content_hash;
    use crate::report::{PatchRisk, ValidationKind};
    use crate::target::ReviewTarget;

    #[derive(Clone, Copy)]
    enum VerdictMode {
        AcceptAll,
        RejectSecond,
    }

    struct FakeSampler {
        candidates: Mutex<Option<SampledCandidates>>,
        mode: VerdictMode,
        candidate_calls: AtomicUsize,
        validation_calls: AtomicUsize,
        validation_request: Mutex<Option<ValidationRequest>>,
        validation_usage: UsageSummary,
    }

    impl FakeSampler {
        fn new(candidates: Vec<ReviewCandidate>, mode: VerdictMode) -> Self {
            Self {
                candidates: Mutex::new(Some(SampledCandidates {
                    submission: CandidateSubmission { candidates },
                    usage: UsageSummary {
                        model: "candidate-model".into(),
                        prompt_tokens: 10,
                        completion_tokens: 4,
                        total_tokens: 14,
                        estimated_cost_usd: Some(0.01),
                    },
                })),
                mode,
                candidate_calls: AtomicUsize::new(0),
                validation_calls: AtomicUsize::new(0),
                validation_request: Mutex::new(None),
                validation_usage: UsageSummary {
                    model: "validator-model".into(),
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    total_tokens: 10,
                    estimated_cost_usd: Some(0.02),
                },
            }
        }
    }

    #[async_trait]
    impl ReviewSampler for FakeSampler {
        async fn sample_candidates(
            &self,
            _request: &CandidateRequest,
        ) -> Result<SampledCandidates, ReviewEngineError> {
            self.candidate_calls.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(self
                .candidates
                .lock()
                .unwrap()
                .take()
                .expect("candidate sampling is expected once"))
        }

        async fn sample_validation(
            &self,
            request: &ValidationRequest,
        ) -> Result<SampledValidation, ReviewEngineError> {
            self.validation_calls.fetch_add(1, AtomicOrdering::Relaxed);
            *self.validation_request.lock().unwrap() = Some(request.clone());
            let verdicts = request
                .candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| ValidationVerdict {
                    candidate_id: candidate.candidate_id.clone(),
                    accepted: !matches!(self.mode, VerdictMode::RejectSecond) || index != 1,
                    confidence: Some(candidate.confidence - 0.05),
                    rationale: "independently confirmed against the supplied evidence".into(),
                })
                .collect();
            Ok(SampledValidation {
                submission: ValidationSubmission { verdicts },
                usage: self.validation_usage.clone(),
            })
        }
    }

    fn location(line: u32) -> CodeLocation {
        CodeLocation {
            path: "src/lib.rs".into(),
            line,
            column: None,
            end_line: None,
        }
    }

    fn fixture() -> (NormalizedDiff, ContextBundle, RepositoryRules, String) {
        let source_hash = content_hash("whole source file");
        let hunk = DiffHunk {
            old_start: 9,
            old_count: 3,
            new_start: 9,
            new_count: 4,
            section: "fn work".into(),
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    content: "before".into(),
                    old_line: Some(9),
                    new_line: Some(9),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    content: "first change".into(),
                    old_line: None,
                    new_line: Some(10),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    content: "second change".into(),
                    old_line: None,
                    new_line: Some(11),
                },
            ],
        };
        let diff = NormalizedDiff {
            target: ReviewTarget::WorkingTree,
            base_revision: Some("base".into()),
            head_revision: None,
            fingerprint: "diff-fingerprint".into(),
            files: vec![ReviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                change: ChangeKind::Modified,
                untracked: false,
                binary: false,
                old_mode: None,
                new_mode: None,
                patch: "patch".into(),
                hunks: vec![hunk],
                content_hash: Some(source_hash.clone()),
            }],
            stats: DiffStats {
                files: 1,
                hunks: 1,
                additions: 2,
                deletions: 0,
                binary_files: 0,
            },
        };
        let context = ContextBundle {
            items: vec![crate::ContextItem {
                path: "src/lib.rs".into(),
                start_line: 7,
                end_line: 13,
                reason: "changed hunk".into(),
                text: "bounded source".into(),
                content_hash: source_hash.clone(),
            }],
            total_bytes: 14,
            fingerprint: "context-fingerprint".into(),
            coverage_notes: vec!["explicit fixture coverage note".into()],
        };
        (diff, context, RepositoryRules::default(), source_hash)
    }

    fn candidate(
        line: u32,
        severity: Severity,
        confidence: f32,
        source_hash: &str,
    ) -> ReviewCandidate {
        ReviewCandidate {
            candidate_id: "model-controlled-id".into(),
            title: format!("Bug on line {line}"),
            category: FindingCategory::Correctness,
            severity,
            confidence,
            primary_location: location(line),
            evidence: vec![CandidateEvidence {
                location: location(9),
                description: "the surrounding control flow proves the failing path".into(),
                content_hash: source_hash.into(),
            }],
            failure_scenario: "a concrete input reaches the incorrect branch".into(),
            assumptions: vec!["the caller accepts this input".into()],
            suggested_action: "guard the branch before using the value".into(),
            validation_plan: vec![ValidationStep {
                kind: ValidationKind::Test,
                description: "run the focused regression test".into(),
                command: Some("cargo test focused".into()),
            }],
            patch_strategy: Some(PatchStrategy {
                summary: "add a localized guard".into(),
                risk: PatchRisk::Low,
                files: vec!["src/lib.rs".into()],
            }),
        }
    }

    #[test]
    fn zero_candidates_is_a_clean_report_and_skips_validation() {
        let (diff, context, rules, _) = fixture();
        let sampler = FakeSampler::new(Vec::new(), VerdictMode::AcceptAll);
        let report = block_on(build_review_report(
            &sampler,
            &diff,
            &context,
            &rules,
            ReviewFocus::All,
            ReviewId("review-zero".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();

        assert!(report.findings.is_empty());
        assert_eq!(report.summary.findings_total, 0);
        assert_eq!(report.summary.files_reviewed, 1);
        assert_eq!(report.summary.hunks_reviewed, 1);
        assert_eq!(report.usage.model, "candidate-model");
        assert_eq!(sampler.candidate_calls.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(sampler.validation_calls.load(AtomicOrdering::Relaxed), 0);
    }

    #[test]
    fn deterministic_prevalidation_rejects_unsupported_candidates() {
        let (diff, context, rules, source_hash) = fixture();
        let good = candidate(10, Severity::High, 0.9, &source_hash);
        let mut unchanged = good.clone();
        unchanged.primary_location = location(9);
        let mut bad_hash = good.clone();
        bad_hash.evidence[0].content_hash = "not-the-source-hash".into();
        let mut invalid_confidence = good.clone();
        invalid_confidence.confidence = f32::NAN;
        let sampler = FakeSampler::new(
            vec![unchanged, bad_hash, invalid_confidence, good],
            VerdictMode::AcceptAll,
        );

        let report = block_on(build_review_report(
            &sampler,
            &diff,
            &context,
            &rules,
            ReviewFocus::All,
            ReviewId("review-prevalidation".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();
        assert_eq!(report.findings.len(), 1);
        assert!(
            report
                .summary
                .coverage_notes
                .iter()
                .any(|note| note.contains("3 candidate(s) were rejected"))
        );
        let request = sampler.validation_request.lock().unwrap();
        let request = request.as_ref().unwrap();
        assert_eq!(request.candidates.len(), 1);
        assert!(request.candidates[0].candidate_id.starts_with("candidate_"));
        assert_ne!(request.candidates[0].candidate_id, "model-controlled-id");
    }

    #[test]
    fn independent_verdicts_fail_closed_and_cannot_raise_confidence() {
        let (diff, context, rules, source_hash) = fixture();
        let sampler = FakeSampler::new(
            vec![
                candidate(10, Severity::High, 0.9, &source_hash),
                candidate(11, Severity::Medium, 0.8, &source_hash),
            ],
            VerdictMode::RejectSecond,
        );
        let report = block_on(build_review_report(
            &sampler,
            &diff,
            &context,
            &rules,
            ReviewFocus::All,
            ReviewId("review-verdicts".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].confidence < 0.9);
        assert!(
            report
                .summary
                .coverage_notes
                .iter()
                .any(|note| note.contains("unconfirmed by independent validation"))
        );
        assert_eq!(report.usage.prompt_tokens, 17);
        assert_eq!(report.usage.completion_tokens, 7);
        assert_eq!(report.usage.total_tokens, 24);
        assert!((report.usage.estimated_cost_usd.unwrap() - 0.03).abs() < f64::EPSILON);
        assert_eq!(report.usage.model, "candidate-model + validator-model");
    }

    #[test]
    fn dedupe_ranking_and_finding_ids_are_order_independent() {
        let (diff, context, rules, source_hash) = fixture();
        let low_duplicate = candidate(10, Severity::Medium, 0.7, &source_hash);
        let mut high_duplicate = candidate(10, Severity::High, 0.95, &source_hash);
        high_duplicate.title = "Preferred duplicate".into();
        let other = candidate(11, Severity::Critical, 0.9, &source_hash);

        let first = FakeSampler::new(
            vec![low_duplicate.clone(), other.clone(), high_duplicate.clone()],
            VerdictMode::AcceptAll,
        );
        let second = FakeSampler::new(
            vec![high_duplicate, other, low_duplicate],
            VerdictMode::AcceptAll,
        );
        let first_report = block_on(build_review_report(
            &first,
            &diff,
            &context,
            &rules,
            ReviewFocus::All,
            ReviewId("review-first".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();
        let second_report = block_on(build_review_report(
            &second,
            &diff,
            &context,
            &rules,
            ReviewFocus::All,
            ReviewId("review-second".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();

        assert_eq!(first_report.findings.len(), 2);
        assert_eq!(first_report.findings[0].severity, Severity::Critical);
        assert_eq!(first_report.findings[1].title, "Preferred duplicate");
        let first_ids: Vec<&str> = first_report
            .findings
            .iter()
            .map(|finding| finding.id.0.as_str())
            .collect();
        let second_ids: Vec<&str> = second_report
            .findings
            .iter()
            .map(|finding| finding.id.0.as_str())
            .collect();
        assert_eq!(first_ids, second_ids);
        assert!(
            first_ids
                .iter()
                .all(|id| id.starts_with("HM-") && id.len() == 15)
        );
        assert!(
            first_report
                .summary
                .coverage_notes
                .iter()
                .any(|note| note.contains("duplicate candidate"))
        );
    }

    #[test]
    fn focused_reviews_discard_other_categories_before_model_validation() {
        let (diff, context, rules, source_hash) = fixture();
        let candidate = candidate(10, Severity::High, 0.9, &source_hash);
        let sampler = FakeSampler::new(vec![candidate], VerdictMode::AcceptAll);
        let report = block_on(build_review_report(
            &sampler,
            &diff,
            &context,
            &rules,
            ReviewFocus::Security,
            ReviewId("review-security".into()),
            "workspace".into(),
            42,
        ))
        .unwrap();
        assert!(report.findings.is_empty());
        assert_eq!(sampler.validation_calls.load(AtomicOrdering::Relaxed), 0);
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
