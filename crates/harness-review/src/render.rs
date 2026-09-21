use std::fmt::Write as _;

use crate::{
    FindingCategory, FindingStatus, PatchRisk, ReviewFinding, ReviewReport, Severity,
    ValidationKind, VerificationOutcome,
};

/// Render one review for an interactive terminal.
///
/// The output deliberately contains no ANSI escapes. Styling belongs to the
/// host UI, while this shared renderer stays deterministic for terminals,
/// redirected files, tests, and editor previews. Untrusted model/repository
/// text has control characters removed so it cannot inject terminal escapes.
pub fn render_terminal(report: &ReviewReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "HiveMind Review {}",
        terminal_text(&report.review_id.0)
    );
    let _ = writeln!(out, "Target: {}", terminal_text(&report.target.to_string()));
    write_revisions_terminal(&mut out, report);
    let _ = writeln!(
        out,
        "Reviewed: {} file(s), {} hunk(s)",
        report.summary.files_reviewed, report.summary.hunks_reviewed
    );
    let _ = writeln!(
        out,
        "Findings: {} ({} critical, {} high, {} medium, {} low, {} info)",
        report.summary.findings_total,
        report.summary.critical,
        report.summary.high,
        report.summary.medium,
        report.summary.low,
        report.summary.info
    );

    if !report.summary.coverage_notes.is_empty() {
        out.push_str("\nCoverage\n");
        for note in &report.summary.coverage_notes {
            let _ = writeln!(out, "- {}", terminal_text(note));
        }
    }

    out.push_str("\nFindings\n");
    if report.findings.is_empty() {
        out.push_str("No defensible findings.\n");
    } else {
        for finding in &report.findings {
            write_finding_terminal(&mut out, finding);
        }
    }

    if let Some(verification) = &report.verification {
        out.push_str("\nVerification\n");
        if verification.results.is_empty() {
            out.push_str("No verification checks were recorded.\n");
        }
        for result in &verification.results {
            let _ = writeln!(
                out,
                "- {} [{} / {}]: {}",
                terminal_text(&result.label),
                verification_label(result.outcome),
                validation_label(result.kind),
                terminal_text(&result.summary)
            );
        }
    }

    write_usage_terminal(&mut out, report);
    out
}

/// Render the same report as portable Markdown.
pub fn render_markdown(report: &ReviewReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HiveMind Review {}\n",
        markdown_text(&report.review_id.0)
    );
    let _ = writeln!(
        out,
        "- Target: {}",
        markdown_text(&report.target.to_string())
    );
    if let Some(base) = &report.base_revision {
        let _ = writeln!(out, "- Base revision: {}", markdown_text(base));
    }
    if let Some(head) = &report.head_revision {
        let _ = writeln!(out, "- Head revision: {}", markdown_text(head));
    }
    let _ = writeln!(
        out,
        "- Reviewed: {} file(s), {} hunk(s)",
        report.summary.files_reviewed, report.summary.hunks_reviewed
    );
    let _ = writeln!(
        out,
        "- Findings: {} ({} critical, {} high, {} medium, {} low, {} info)",
        report.summary.findings_total,
        report.summary.critical,
        report.summary.high,
        report.summary.medium,
        report.summary.low,
        report.summary.info
    );

    if !report.summary.coverage_notes.is_empty() {
        out.push_str("\n## Coverage\n\n");
        for note in &report.summary.coverage_notes {
            let _ = writeln!(out, "- {}", markdown_text(note));
        }
    }

    out.push_str("\n## Findings\n\n");
    if report.findings.is_empty() {
        out.push_str("No defensible findings.\n");
    } else {
        for finding in &report.findings {
            write_finding_markdown(&mut out, finding);
        }
    }

    if let Some(verification) = &report.verification {
        out.push_str("\n## Verification\n\n");
        if verification.results.is_empty() {
            out.push_str("No verification checks were recorded.\n");
        }
        for result in &verification.results {
            let _ = writeln!(
                out,
                "- **{}** — {} / {}: {}",
                markdown_text(&result.label),
                verification_label(result.outcome),
                validation_label(result.kind),
                markdown_text(&result.summary)
            );
        }
    }

    write_usage_markdown(&mut out, report);
    out
}

fn write_revisions_terminal(out: &mut String, report: &ReviewReport) {
    if let Some(base) = &report.base_revision {
        let _ = writeln!(out, "Base revision: {}", terminal_text(base));
    }
    if let Some(head) = &report.head_revision {
        let _ = writeln!(out, "Head revision: {}", terminal_text(head));
    }
}

fn write_finding_terminal(out: &mut String, finding: &ReviewFinding) {
    let _ = writeln!(
        out,
        "\n{}  {}  {}  {}% confidence",
        terminal_text(&finding.id.0),
        severity_label(finding.severity),
        category_label(finding.category),
        confidence_percent(finding.confidence)
    );
    let _ = writeln!(out, "{}", terminal_text(&finding.title));
    let _ = writeln!(
        out,
        "Location: {}:{}",
        terminal_text(&finding.primary_location.path),
        finding.primary_location.line
    );
    let _ = writeln!(out, "Status: {}", status_label(finding.status));
    let _ = writeln!(
        out,
        "Failure scenario: {}",
        terminal_text(&finding.failure_scenario)
    );

    if !finding.evidence.is_empty() {
        out.push_str("Evidence:\n");
        for evidence in &finding.evidence {
            let _ = writeln!(
                out,
                "- {}:{} — {}",
                terminal_text(&evidence.location.path),
                evidence.location.line,
                terminal_text(&evidence.description)
            );
        }
    }
    if !finding.assumptions.is_empty() {
        out.push_str("Assumptions:\n");
        for assumption in &finding.assumptions {
            let _ = writeln!(out, "- {}", terminal_text(assumption));
        }
    }
    let _ = writeln!(
        out,
        "Suggested action: {}",
        terminal_text(&finding.suggested_action)
    );
    if let Some(strategy) = &finding.patch_strategy {
        let _ = writeln!(
            out,
            "Patch strategy ({} risk): {}",
            patch_risk_label(strategy.risk),
            terminal_text(&strategy.summary)
        );
    }
    if !finding.validation_plan.is_empty() {
        out.push_str("Validation plan:\n");
        for step in &finding.validation_plan {
            let command = step
                .command
                .as_deref()
                .map(|value| format!(" — {}", terminal_text(value)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- {}: {}{}",
                validation_label(step.kind),
                terminal_text(&step.description),
                command
            );
        }
    }
}

fn write_finding_markdown(out: &mut String, finding: &ReviewFinding) {
    let _ = writeln!(
        out,
        "### {} — {}: {}\n",
        markdown_text(&finding.id.0),
        severity_label(finding.severity),
        markdown_text(&finding.title)
    );
    let _ = writeln!(out, "- Category: {}", category_label(finding.category));
    let _ = writeln!(
        out,
        "- Confidence: {}%",
        confidence_percent(finding.confidence)
    );
    let _ = writeln!(
        out,
        "- Location: {}:{}",
        markdown_text(&finding.primary_location.path),
        finding.primary_location.line
    );
    let _ = writeln!(out, "- Status: {}", status_label(finding.status));
    let _ = writeln!(
        out,
        "\n**Failure scenario:** {}\n",
        markdown_text(&finding.failure_scenario)
    );

    if !finding.evidence.is_empty() {
        out.push_str("**Evidence:**\n\n");
        for evidence in &finding.evidence {
            let _ = writeln!(
                out,
                "- {}:{} — {}",
                markdown_text(&evidence.location.path),
                evidence.location.line,
                markdown_text(&evidence.description)
            );
        }
        out.push('\n');
    }
    if !finding.assumptions.is_empty() {
        out.push_str("**Assumptions:**\n\n");
        for assumption in &finding.assumptions {
            let _ = writeln!(out, "- {}", markdown_text(assumption));
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "**Suggested action:** {}\n",
        markdown_text(&finding.suggested_action)
    );
    if let Some(strategy) = &finding.patch_strategy {
        let _ = writeln!(
            out,
            "**Patch strategy ({} risk):** {}\n",
            patch_risk_label(strategy.risk),
            markdown_text(&strategy.summary)
        );
    }
    if !finding.validation_plan.is_empty() {
        out.push_str("**Validation plan:**\n\n");
        for step in &finding.validation_plan {
            let command = step
                .command
                .as_deref()
                .map(|value| format!(" — {}", markdown_text(value)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- {}: {}{}",
                validation_label(step.kind),
                markdown_text(&step.description),
                command
            );
        }
        out.push('\n');
    }
}

fn write_usage_terminal(out: &mut String, report: &ReviewReport) {
    out.push_str("\nUsage\n");
    let model = if report.usage.model.is_empty() {
        "not reported".to_string()
    } else {
        terminal_text(&report.usage.model)
    };
    let _ = writeln!(out, "Model: {model}");
    let _ = writeln!(
        out,
        "Tokens: {} prompt, {} completion, {} total",
        report.usage.prompt_tokens, report.usage.completion_tokens, report.usage.total_tokens
    );
    if let Some(cost) = report.usage.estimated_cost_usd {
        let _ = writeln!(out, "Estimated cost: ${cost:.4}");
    }
}

fn write_usage_markdown(out: &mut String, report: &ReviewReport) {
    out.push_str("\n## Usage\n\n");
    let model = if report.usage.model.is_empty() {
        "not reported".to_string()
    } else {
        markdown_text(&report.usage.model)
    };
    let _ = writeln!(out, "- Model: {model}");
    let _ = writeln!(
        out,
        "- Tokens: {} prompt, {} completion, {} total",
        report.usage.prompt_tokens, report.usage.completion_tokens, report.usage.total_tokens
    );
    if let Some(cost) = report.usage.estimated_cost_usd {
        let _ = writeln!(out, "- Estimated cost: ${cost:.4}");
    }
}

fn confidence_percent(confidence: f32) -> u8 {
    if !confidence.is_finite() {
        return 0;
    }
    (confidence.clamp(0.0, 1.0) * 100.0).round() as u8
}

const fn severity_label(value: Severity) -> &'static str {
    match value {
        Severity::Info => "INFO",
        Severity::Low => "LOW",
        Severity::Medium => "MEDIUM",
        Severity::High => "HIGH",
        Severity::Critical => "CRITICAL",
    }
}

const fn category_label(value: FindingCategory) -> &'static str {
    match value {
        FindingCategory::Correctness => "correctness",
        FindingCategory::Security => "security",
        FindingCategory::Performance => "performance",
        FindingCategory::Maintainability => "maintainability",
        FindingCategory::TestGap => "test gap",
    }
}

const fn status_label(value: FindingStatus) -> &'static str {
    match value {
        FindingStatus::Open => "open",
        FindingStatus::Ignored => "ignored",
        FindingStatus::FixPreviewed => "fix previewed",
        FindingStatus::Fixed => "fixed",
        FindingStatus::VerificationFailed => "verification failed",
        FindingStatus::Stale => "stale",
    }
}

const fn patch_risk_label(value: PatchRisk) -> &'static str {
    match value {
        PatchRisk::Low => "low",
        PatchRisk::Medium => "medium",
        PatchRisk::High => "high",
    }
}

const fn validation_label(value: ValidationKind) -> &'static str {
    match value {
        ValidationKind::Syntax => "syntax",
        ValidationKind::Format => "format",
        ValidationKind::Lint => "lint",
        ValidationKind::Typecheck => "typecheck",
        ValidationKind::Test => "test",
        ValidationKind::Custom => "custom",
    }
}

const fn verification_label(value: VerificationOutcome) -> &'static str {
    match value {
        VerificationOutcome::Passed => "passed",
        VerificationOutcome::ExistingFailure => "existing failure",
        VerificationOutcome::IntroducedFailure => "introduced failure",
        VerificationOutcome::ResolvedFailure => "resolved failure",
        VerificationOutcome::Inconclusive => "inconclusive",
        VerificationOutcome::TimedOut => "timed out",
        VerificationOutcome::NotConfigured => "not configured",
    }
}

fn terminal_text(value: &str) -> String {
    clean_inline(value)
}

fn markdown_text(value: &str) -> String {
    let clean = clean_inline(value);
    let mut out = String::with_capacity(clean.len());
    for ch in clean.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '`' | '*' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '!' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn clean_inline(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CodeLocation, EvidenceItem, FindingId, ReviewId, ReviewSummary, ReviewTarget, UsageSummary,
        ValidationStep,
    };

    fn report() -> ReviewReport {
        let finding = ReviewFinding {
            id: FindingId("HM-001".into()),
            title: "Retry can charge twice".into(),
            category: FindingCategory::Correctness,
            severity: Severity::High,
            confidence: 0.92,
            priority_score: 3.5,
            primary_location: CodeLocation {
                path: "src/payments.rs".into(),
                line: 18,
                column: None,
                end_line: Some(20),
            },
            source_hash: "source-hash".into(),
            evidence: vec![EvidenceItem {
                location: CodeLocation {
                    path: "src/provider.rs".into(),
                    line: 42,
                    column: None,
                    end_line: None,
                },
                description: "request ID is the idempotency key".into(),
                content_hash: "evidence-hash".into(),
            }],
            failure_scenario: "a timeout retries with a fresh request ID".into(),
            assumptions: vec!["the provider may accept before timing out".into()],
            suggested_action: "reuse one request ID across retries".into(),
            validation_plan: vec![ValidationStep {
                kind: ValidationKind::Test,
                description: "run payment tests".into(),
                command: Some("cargo test payments".into()),
            }],
            patch_strategy: Some(crate::PatchStrategy {
                summary: "move ID creation before the retry loop".into(),
                risk: PatchRisk::Low,
                files: vec!["src/payments.rs".into()],
            }),
            status: FindingStatus::Open,
        };
        let mut summary = ReviewSummary {
            files_reviewed: 2,
            hunks_reviewed: 3,
            coverage_notes: vec!["binary assets were skipped".into()],
            ..ReviewSummary::default()
        };
        summary.count_findings(std::slice::from_ref(&finding));
        ReviewReport::new(
            ReviewId("review_123".into()),
            ReviewTarget::WorkingTree,
            Some("base123".into()),
            None,
            "diff-hash".into(),
            "workspace-hash".into(),
            summary,
            vec![finding],
            UsageSummary {
                model: "hivemind".into(),
                prompt_tokens: 100,
                completion_tokens: 25,
                total_tokens: 125,
                estimated_cost_usd: Some(0.0123),
            },
            42,
        )
    }

    #[test]
    fn terminal_and_markdown_project_the_same_report_facts() {
        let report = report();
        let terminal = render_terminal(&report);
        let markdown = render_markdown(&report);

        for fact in [
            "review_123",
            "HM-001",
            "Retry can charge twice",
            "src/payments.rs",
            "a timeout retries with a fresh request ID",
            "request ID is the idempotency key",
            "reuse one request ID across retries",
            "binary assets were skipped",
            "hivemind",
            "$0.0123",
        ] {
            assert!(
                terminal.contains(fact),
                "terminal omitted {fact:?}:\n{terminal}"
            );
            assert!(
                markdown.contains(fact),
                "markdown omitted {fact:?}:\n{markdown}"
            );
        }
    }

    #[test]
    fn a_clean_review_has_an_explicit_zero_findings_message() {
        let mut report = report();
        report.findings.clear();
        report.summary.count_findings(&[]);
        assert!(render_terminal(&report).contains("No defensible findings."));
        assert!(render_markdown(&report).contains("No defensible findings."));
    }

    #[test]
    fn untrusted_text_cannot_inject_terminal_escapes_or_raw_markdown_html() {
        let mut report = report();
        report.findings[0].title = "\u{1b}[31m<script>alert(1)</script>\nheading".into();
        let terminal = render_terminal(&report);
        let markdown = render_markdown(&report);
        assert!(!terminal.contains('\u{1b}'));
        assert!(!markdown.contains("<script>"));
        assert!(markdown.contains("&lt;script&gt;"));
    }

    #[test]
    fn confidence_is_clamped_for_defensive_rendering() {
        let mut report = report();
        report.findings[0].confidence = 9.0;
        assert!(render_terminal(&report).contains("100% confidence"));
        report.findings[0].confidence = f32::NAN;
        assert!(render_terminal(&report).contains("0% confidence"));
    }
}
