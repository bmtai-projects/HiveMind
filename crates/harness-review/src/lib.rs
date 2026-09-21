//! Review-domain types and deterministic, read-only Git diff acquisition.
//!
//! This crate deliberately has no dependency on the CLI, provider, or agent
//! loop. A terminal, editor, CI runner, or future GitHub service can consume
//! the same normalized target and report shapes without duplicating review
//! semantics or taking a dependency on a presentation layer.

mod candidate;
mod context;
mod diff;
mod event;
mod hash;
mod render;
mod report;
mod store;
mod target;

pub use candidate::{
    CandidateEvidence, CandidateRequest, CandidateSubmission, ReviewCandidate, ReviewEngineError,
    ReviewFocus, ReviewSampler, SampledCandidates, SampledValidation, ValidationRequest,
    ValidationSubmission, ValidationVerdict, build_review_report,
};
pub use context::{
    ContextBundle, ContextError, ContextItem, ContextLimits, RepositoryRules, RuleDocument,
    build_context, load_repository_rules,
};
pub use diff::{
    ChangeKind, DiffHunk, DiffLimits, DiffLine, DiffLineKind, DiffStats, GitRepository,
    NormalizedDiff, ReviewError, ReviewFile,
};
pub use event::ReviewEventEnvelope;
pub use hash::{content_hash, stable_hash};
pub use render::{render_markdown, render_terminal};
pub use report::{
    CodeLocation, EvidenceItem, FindingCategory, FindingId, FindingStatus, PatchRisk,
    PatchStrategy, ReviewFinding, ReviewId, ReviewReport, ReviewSummary, Severity, UsageSummary,
    ValidationKind, ValidationStep, VerificationOutcome, VerificationReport, VerificationResult,
};
pub use store::{ReviewStore, StoreError};
pub use target::{ReviewTarget, ScmProvider};

/// Version of serialized reports and streamed review event envelopes.
pub const REVIEW_SCHEMA_VERSION: &str = "1.0";
