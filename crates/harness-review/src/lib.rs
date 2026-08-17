//! Review-domain types and deterministic, read-only Git diff acquisition.
//!
//! This crate deliberately has no dependency on the CLI, provider, or agent
//! loop. A terminal, editor, CI runner, or future GitHub service can consume
//! the same normalized target and report shapes without duplicating review
//! semantics or taking a dependency on a presentation layer.

mod diff;
mod hash;
mod report;
mod target;

pub use diff::{
    ChangeKind, DiffHunk, DiffLimits, DiffLine, DiffLineKind, DiffStats, GitRepository,
    NormalizedDiff, ReviewError, ReviewFile,
};
pub use hash::{content_hash, stable_hash};
pub use report::{
    CodeLocation, EvidenceItem, FindingCategory, FindingId, FindingStatus, PatchRisk,
    PatchStrategy, ReviewFinding, ReviewId, ReviewReport, ReviewSummary, Severity, UsageSummary,
    ValidationKind, ValidationStep, VerificationOutcome, VerificationReport, VerificationResult,
};
pub use target::{ReviewTarget, ScmProvider};

/// Version of serialized reports and streamed review event envelopes.
pub const REVIEW_SCHEMA_VERSION: &str = "1.0";
