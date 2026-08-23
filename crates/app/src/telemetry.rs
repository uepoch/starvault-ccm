//! Error telemetry (decision S3): opt-in, crash-plus-error capture.
//!
//! Initialized at startup only when the user opted in. Captures Rust panics
//! (sentry's panic handler), and every command failure reported through
//! `capture` — which `log_op` calls for `error`-level entries — so users
//! never need to file a bug report for us to see what broke. No breadcrumbs,
//! no PII beyond what the error text itself contains.

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};

/// Client DSN: a public identifier, not a secret (Sentry docs).
const DSN: &str = "https://80d8b882a57a9456e30943ab7d5bbf37@o4511954327175169.ingest.de.sentry.io/4511954358304848";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Start Sentry when the user opted in. Idempotent: `set_enabled` flips it
/// live at runtime when the toggle changes.
pub fn init(app: &AppHandle) {
    // reqwest (Sentry transport + updater) lands on rustls-no-provider via
    // the tauri dependency chain; the ring provider is compiled but must be
    // installed as the default before any Client is built.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Ok(cfg) = crate::commands::load_config(&app.state::<crate::commands::AppState>()) {
        set_enabled(cfg.crash_reports_opt_in);
    }
}

/// Flip telemetry at runtime (Settings toggle); initializes on first enable.
pub fn set_enabled(on: bool) {
    if ENABLED.swap(on, Ordering::Relaxed) == on {
        return; // already in the requested state: no second client
    }
    if on {
        let _guard = sentry::init((
            DSN,
            sentry::ClientOptions::default().traces_sample_rate(1.0),
        ));
        // The guard would shut the client down on drop; leak it to keep the
        // client alive for the process lifetime.
        std::mem::forget(_guard);
    }
}

/// Report a user-visible failure (called for every error-level operation-log
/// entry). Cheap no-op when telemetry is off.
pub fn capture(error: &str) {
    if ENABLED.load(Ordering::Relaxed) {
        sentry::capture_message(error, sentry::Level::Error);
    }
}
