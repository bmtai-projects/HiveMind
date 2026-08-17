use serde::{Deserialize, Serialize};

use crate::{REVIEW_SCHEMA_VERSION, ReviewTarget};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FindingId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    Correctness,
    Security,
    Performance,
    Maintainability,
    TestGap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const fn weight(self) -> f32 {
        match self {
            Self::Info => 0.5,
            Self::Low => 1.0,
            Self::Medium => 2.0,
            Self::High => 4.0,
            Self::Critical => 8.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeLocation {
    pub path: String,
    /// One-indexed line number.
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub location: CodeLocation,
    pub description: String,
    /// Hash of the complete source file from which this evidence was
    /// collected, not merely the displayed excerpt.
    pub content_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationKind {
    Syntax,
    Format,
    Lint,
    Typecheck,
    Test,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationStep {
    pub kind: ValidationKind,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchRisk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchStrategy {
    pub summary: String,
    pub risk: PatchRisk,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Open,
    Ignored,
    FixPreviewed,
    Fixed,
    VerificationFailed,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: FindingId,
    pub title: String,
    pub category: FindingCategory,
    pub severity: Severity,
    /// Normalized to the inclusive range `0.0..=1.0` by the validator.
    pub confidence: f32,
    pub priority_score: f32,
    pub primary_location: CodeLocation,
    /// Hash of the complete primary source file at review time.
    pub source_hash: String,
    pub evidence: Vec<EvidenceItem>,
    pub failure_scenario: String,
    #[serde(default)]
    pub assumptions: Vec<String>,
    pub suggested_action: String,
    #[serde(default)]
    pub validation_plan: Vec<ValidationStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_strategy: Option<PatchStrategy>,
    pub status: FindingStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub files_reviewed: usize,
    pub hunks_reviewed: usize,
    pub findings_total: usize,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub info: usize,
    #[serde(default)]
    pub coverage_notes: Vec<String>,
}

impl ReviewSummary {
    pub fn count_findings(&mut self, findings: &[ReviewFinding]) {
        self.findings_total = findings.len();
        self.critical = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Critical)
            .count();
        self.high = findings
            .iter()
            .filter(|finding| finding.severity == Severity::High)
            .count();
        self.medium = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Medium)
            .count();
        self.low = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Low)
            .count();
        self.info = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Info)
            .count();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOutcome {
    Passed,
    ExistingFailure,
    IntroducedFailure,
    ResolvedFailure,
    Inconclusive,
    TimedOut,
    NotConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub label: String,
    pub kind: ValidationKind,
    pub command: String,
    pub outcome: VerificationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_fix_exit_code: Option<i32>,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub results: Vec<VerificationResult>,
    pub introduced_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub schema_version: String,
    pub review_id: ReviewId,
    pub target: ReviewTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<String>,
    pub diff_fingerprint: String,
    pub workspace_fingerprint: String,
    pub summary: ReviewSummary,
    pub findings: Vec<ReviewFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationReport>,
    pub usage: UsageSummary,
    /// Unix timestamp in seconds. Avoiding a transport-specific date type
    /// keeps the domain crate small and every output format unambiguous.
    pub created_at: u64,
}

impl ReviewReport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        review_id: ReviewId,
        target: ReviewTarget,
        base_revision: Option<String>,
        head_revision: Option<String>,
        diff_fingerprint: String,
        workspace_fingerprint: String,
        summary: ReviewSummary,
        findings: Vec<ReviewFinding>,
        usage: UsageSummary,
        created_at: u64,
    ) -> Self {
        Self {
            schema_version: REVIEW_SCHEMA_VERSION.to_string(),
            review_id,
            target,
            base_revision,
            head_revision,
            diff_fingerprint,
            workspace_fingerprint,
            summary,
            findings,
            verification: None,
            usage,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_schema_is_explicit_and_round_trips() {
        let report = ReviewReport::new(
            ReviewId("review_abc".into()),
            ReviewTarget::WorkingTree,
            Some("base".into()),
            None,
            "diff".into(),
            "workspace".into(),
            ReviewSummary::default(),
            Vec::new(),
            UsageSummary::default(),
            42,
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""schema_version":"1.0""#));
        let decoded: ReviewReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.schema_version, REVIEW_SCHEMA_VERSION);
        assert_eq!(decoded.target, ReviewTarget::WorkingTree);
    }

    #[test]
    fn summary_recounts_each_severity_without_accumulating() {
        fn finding(id: &str, severity: Severity) -> ReviewFinding {
            ReviewFinding {
                id: FindingId(id.into()),
                title: "title".into(),
                category: FindingCategory::Correctness,
                severity,
                confidence: 0.9,
                priority_score: 1.0,
                primary_location: CodeLocation {
                    path: "a.rs".into(),
                    line: 1,
                    column: None,
                    end_line: None,
                },
                source_hash: "hash".into(),
                evidence: Vec::new(),
                failure_scenario: "scenario".into(),
                assumptions: Vec::new(),
                suggested_action: "fix".into(),
                validation_plan: Vec::new(),
                patch_strategy: None,
                status: FindingStatus::Open,
            }
        }
        let findings = vec![
            finding("one", Severity::High),
            finding("two", Severity::Low),
        ];
        let mut summary = ReviewSummary::default();
        summary.count_findings(&findings);
        summary.count_findings(&findings);
        assert_eq!(summary.findings_total, 2);
        assert_eq!(summary.high, 1);
        assert_eq!(summary.low, 1);
    }
}
