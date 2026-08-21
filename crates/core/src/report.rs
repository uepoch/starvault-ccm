//! Crash reporting seam (decision S3).
//!
//! Opt-in, crash-only. The backend vendor is chosen at release; until then the
//! no-op sink swallows everything. The frontend feeds error boundaries into
//! the same seam via a shell command.

use std::sync::{Arc, Mutex};

pub trait ReportSink: Send + Sync {
    fn report_error(&self, context: &str, detail: &str);
}

/// Default sink: records nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl ReportSink for NoopSink {
    fn report_error(&self, _context: &str, _detail: &str) {}
}

static SINK: Mutex<Option<Arc<dyn ReportSink>>> = Mutex::new(None);

/// Install the process-wide sink. Called by the shell when telemetry is
/// enabled; never called otherwise.
pub fn set_sink(sink: Arc<dyn ReportSink>) {
    *SINK.lock().expect("report sink mutex poisoned") = Some(sink);
}

/// Report an internal error. Cheap no-op unless a sink is installed and the
/// user opted in (the shell refuses to install sinks when opted out).
pub fn report_error(context: &str, detail: &str) {
    if let Some(sink) = SINK.lock().expect("report sink mutex poisoned").as_ref() {
        sink.report_error(context, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink(AtomicUsize);
    impl ReportSink for CountingSink {
        fn report_error(&self, _context: &str, _detail: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn noop_by_default_then_counts_once_installed() {
        // No sink installed: must not panic.
        report_error("t", "nothing installed");

        let counter = Arc::new(CountingSink(AtomicUsize::new(0)));
        set_sink(counter.clone());
        report_error("ctx", "detail");
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }
}
