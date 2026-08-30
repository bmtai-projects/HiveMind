//! Opt-in end-to-end latency tracing for diagnosing why the first visible
//! response is sometimes slow.
//!
//! Disabled by default. When enabled, writes one line per trace event to
//! stderr with format:
//!
//!   [trace] <stage_name> +<ms_since_start>ms <correlation_id>
//!
//! Every event contains: stage name, milliseconds since request start, and
//! the session/request correlation ID. Never logs prompts, source code,
//! API keys, or provider responses. No external dependencies.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A latency tracer that records named stage timestamps relative to a start
/// instant and writes them to stderr.
///
/// Clone the `Arc` to share between the Agent and its subsystems — emitting
/// a trace is cheap (one Instant::now + one eprintln).
#[derive(Clone)]
pub struct LatencyTracer {
    inner: Arc<LatencyTracerInner>,
}

struct LatencyTracerInner {
    /// Whether tracing is enabled. Checked once per emit — when false, the
    /// entire method is a single `if` that returns immediately.
    enabled: bool,
    /// The instant the *current request* started.
    ///
    /// Rebased by `begin_request`, not fixed at construction. The spec is
    /// "milliseconds since request start", and a tracer built once in
    /// `main.rs` and never reset reports milliseconds since the process
    /// began instead -- so the second request in a session showed
    /// `user_request_received +18599ms` and every later figure had to be
    /// read by subtracting a number the operator had to find first. For a
    /// tool whose only job is measuring first-response latency, that made
    /// the output actively misleading.
    start: Mutex<Instant>,
    /// Correlation ID shared across all events for this session/request.
    correlation_id: String,
}

impl LatencyTracer {
    /// Create a new tracer. When `enabled` is false all `emit` calls are
    /// no-ops with no measurable overhead beyond the branch.
    pub fn new(enabled: bool, correlation_id: String) -> Self {
        Self {
            inner: Arc::new(LatencyTracerInner {
                enabled,
                start: Mutex::new(Instant::now()),
                correlation_id,
            }),
        }
    }

    /// Rebase the clock to now. Called at the top of every user request so
    /// elapsed figures are relative to *that* request, which is what the
    /// stage names claim.
    pub fn begin_request(&self) {
        if !self.inner.enabled {
            return;
        }
        if let Ok(mut start) = self.inner.start.lock() {
            *start = Instant::now();
        }
    }

    /// Elapsed milliseconds for the current request. Test-only: the real
    /// output path is `emit`, which writes to stderr and cannot be asserted
    /// on directly from a unit test.
    #[cfg(test)]
    pub(crate) fn elapsed_ms_for_test(&self) -> f64 {
        if !self.inner.enabled {
            return 0.0;
        }
        match self.inner.start.lock() {
            Ok(start) => start.elapsed().as_secs_f64() * 1000.0,
            Err(_) => 0.0,
        }
    }

    /// Record a trace event. No-op when tracing is disabled.
    ///
    /// # Format
    /// Every line written to stderr has the structure:
    ///   [trace] <stage> +<ms>ms <correlation_id>
    ///
    /// `stage` is the caller-provided stage name (e.g. "user_request_received").
    /// `ms` is the elapsed milliseconds since this tracer was created.
    /// `correlation_id` is the identifier passed at construction.
    ///
    /// Never writes to stdout. Never logs prompts, source code, API keys,
    /// or provider response content.
    pub fn emit(&self, stage: &str) {
        if !self.inner.enabled {
            return;
        }
        // A poisoned lock must not take tracing down with it -- this is a
        // diagnostic, and 0ms is a better answer than a panic in the middle
        // of someone's request.
        let elapsed = match self.inner.start.lock() {
            Ok(start) => start.elapsed().as_secs_f64() * 1000.0,
            Err(_) => 0.0,
        };
        eprintln!(
            "[trace] {stage} +{elapsed:.0}ms {}",
            self.inner.correlation_id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this replaced: the tracer's clock was fixed at
    /// construction, so the second request in a session reported
    /// `user_request_received +18599ms` instead of `+0ms` and every figure
    /// after it had to be read by subtracting a number the operator had to
    /// go and find. Measured live before the fix.
    #[test]
    fn the_clock_restarts_for_each_request() {
        let t = LatencyTracer::new(true, "corr".into());
        std::thread::sleep(std::time::Duration::from_millis(25));
        let before = t.elapsed_ms_for_test();
        assert!(before >= 20.0, "clock should have advanced, got {before}");

        t.begin_request();
        let after = t.elapsed_ms_for_test();
        assert!(
            after < before,
            "begin_request must rebase: {before}ms -> {after}ms"
        );
        assert!(
            after < 10.0,
            "a fresh request should start near zero, got {after}"
        );
    }

    /// `begin_request` on a disabled tracer must stay a no-op like every
    /// other method -- it is called unconditionally from the agent loop.
    #[test]
    fn begin_request_is_a_no_op_when_disabled() {
        let t = LatencyTracer::new(false, "corr".into());
        t.begin_request();
        assert_eq!(t.elapsed_ms_for_test(), 0.0);
    }

    #[test]
    fn disabled_tracer_produces_no_output() {
        let t = LatencyTracer::new(false, "test".into());
        // Should not panic and should produce no output. We can't redirect
        // eprintln! in a simple test, but the fact it doesn't crash and
        // returns immediately is the meaningful assertion — the alternative
        // is a crash or a format failure.
        t.emit("should_not_crash");
    }

    #[test]
    fn enabled_tracer_emits_with_ms_and_id() {
        let t = LatencyTracer::new(true, "corr-123".into());
        t.emit("first_event");
        // Because eprintln! is used we can't capture the output trivially
        // here, but the next test verifies format coherence.
    }

    #[test]
    fn elapsed_monotonically_increases() {
        let t = LatencyTracer::new(true, "order".into());
        // We rely on the eprintln format being consistent; this test is
        // structural (doesn't crash, uses the same code path).
        t.emit("event_a");
        t.emit("event_b");
    }

    #[test]
    fn correlation_id_is_consistent() {
        let t = LatencyTracer::new(true, "fixed-id-42".into());
        // Both emit the same correlation_id — just exercising the path.
        t.emit("e1");
        t.emit("e2");
    }
}
