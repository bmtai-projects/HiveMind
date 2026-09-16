

use std::sync::{Arc, Mutex};
use std::time::Instant;


#[derive(Clone)]
pub struct LatencyTracer {
    inner: Arc<LatencyTracerInner>,
}

struct LatencyTracerInner {
    enabled: bool,
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

   
    pub fn begin_request(&self) {
        if !self.inner.enabled {
            return;
        }
        if let Ok(mut start) = self.inner.start.lock() {
            *start = Instant::now();
        }
    }

    
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

    
    pub fn emit(&self, stage: &str) {
        if !self.inner.enabled {
            return;
        }
        
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

    #[test]
    fn begin_request_is_a_no_op_when_disabled() {
        let t = LatencyTracer::new(false, "corr".into());
        t.begin_request();
        assert_eq!(t.elapsed_ms_for_test(), 0.0);
    }

    #[test]
    fn disabled_tracer_produces_no_output() {
        let t = LatencyTracer::new(false, "test".into());
        t.emit("should_not_crash");
    }

    #[test]
    fn enabled_tracer_emits_with_ms_and_id() {
        let t = LatencyTracer::new(true, "corr-123".into());
        t.emit("first_event");
    }

    #[test]
    fn elapsed_monotonically_increases() {
        let t = LatencyTracer::new(true, "order".into());
        t.emit("event_a");
        t.emit("event_b");
    }

    #[test]
    fn correlation_id_is_consistent() {
        let t = LatencyTracer::new(true, "fixed-id-42".into());
        t.emit("e1");
        t.emit("e2");
    }
}
