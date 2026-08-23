//! Strict opt-in error telemetry.
//!
//! Sentry receives panics and explicit core `Internal` errors only. Local
//! operation logs retain diagnostic chains; remote events carry a generic
//! message and safe operation/error-code tags.

use std::sync::Mutex;

use sentry::protocol::{Event, Stacktrace};
use tauri::{AppHandle, Manager};

/// Client DSN. This is a public routing identifier, not a credential.
const DSN: &str = "https://80d8b882a57a9456e30943ab7d5bbf37@o4511954327175169.ingest.de.sentry.io/4511954358304848";

#[derive(Default)]
pub struct TelemetryState {
    inner: Mutex<TelemetryInner>,
}

#[derive(Default)]
struct TelemetryInner {
    guard: Option<sentry::ClientInitGuard>,
    enabled: bool,
    generation: u64,
}

impl TelemetryState {
    pub fn set_enabled(&self, enabled: bool) {
        self.set_enabled_with_options(enabled, || client_options(Some(DSN)));
    }

    fn set_enabled_with_options(
        &self,
        enabled: bool,
        options: impl FnOnce() -> sentry::ClientOptions,
    ) {
        let mut inner = self.inner.lock().expect("telemetry state poisoned");
        if inner.enabled == enabled {
            return;
        }
        if enabled {
            inner.guard = Some(sentry::init(options()));
            inner.enabled = true;
            inner.generation += 1;
        } else {
            // Stop new captures before the guard drains and closes transport.
            sentry::Hub::current().bind_client(None);
            inner.enabled = false;
            inner.guard.take();
        }
    }

    #[cfg(test)]
    fn enabled(&self) -> bool {
        self.inner.lock().expect("telemetry state poisoned").enabled
    }

    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.inner
            .lock()
            .expect("telemetry state poisoned")
            .generation
    }
}

fn client_options(dsn: Option<&str>) -> sentry::ClientOptions {
    let mut options = sentry::ClientOptions::default();
    options.dsn = dsn.and_then(|value| value.parse().ok());
    options.send_default_pii = false;
    options.auto_session_tracking = false;
    options.enable_logs = false;
    options.enable_metrics = false;
    options
        .traces_sample_rate(0.0)
        .before_send(|event| Some(sanitize_event(event)))
}

/// Apply persisted consent after application state exists.
pub fn init(app: &AppHandle) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Ok(config) = crate::commands::load_config(&app.state::<crate::commands::AppState>()) {
        app.state::<crate::commands::AppState>()
            .telemetry
            .set_enabled(config.crash_reports_opt_in);
    }
}

/// Report one explicit internal failure and return its local report id.
pub fn capture_internal(
    state: &TelemetryState,
    operation: &str,
    error: &svccm_core::Error,
) -> Option<String> {
    capture_internal_with(state, operation, error, |operation, code| {
        let event_id = sentry::with_scope(
            |scope| {
                scope.set_tag("operation", operation);
                scope.set_tag("error_code", code);
            },
            || sentry::capture_message("internal_failure", sentry::Level::Error),
        );
        event_id.to_string()
    })
}

fn capture_internal_with(
    state: &TelemetryState,
    operation: &str,
    error: &svccm_core::Error,
    send: impl FnOnce(String, String) -> String,
) -> Option<String> {
    if error.kind() != svccm_core::ErrorKind::Internal {
        return None;
    }
    // Keep consent synchronized through the capture itself. Otherwise a
    // disable could win after an optimistic `enabled()` check and an event
    // could still be submitted after the user opted out.
    let inner = state.inner.lock().expect("telemetry state poisoned");
    if !inner.enabled {
        return None;
    }
    let report_id = send(safe_tag(operation), safe_tag(error.code()));
    drop(inner);
    Some(report_id)
}

fn safe_tag(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect()
}

/// Remove local values even from panic events created by Sentry integrations.
fn sanitize_event(mut event: Event<'static>) -> Event<'static> {
    event.message = event.message.map(|_| "failure payload redacted".into());
    event.logentry = None;
    event.logger = None;
    event.culprit = None;
    event.transaction = None;
    event.server_name = None;
    event.modules.clear();
    event.user = None;
    event.request = None;
    event.breadcrumbs.values.clear();
    event.contexts.clear();
    event.extra.clear();
    event.template = None;
    event.debug_meta = std::borrow::Cow::Owned(Default::default());
    event
        .tags
        .retain(|key, _| matches!(key.as_str(), "operation" | "error_code"));
    for exception in &mut event.exception.values {
        exception.value = exception
            .value
            .as_ref()
            .map(|_| "panic payload redacted".into());
        if let Some(mechanism) = &mut exception.mechanism {
            mechanism.description = None;
            mechanism.help_link = None;
            mechanism.data.clear();
        }
        exception.thread_id = None;
        sanitize_stacktrace(exception.stacktrace.as_mut());
        sanitize_stacktrace(exception.raw_stacktrace.as_mut());
    }
    sanitize_stacktrace(event.stacktrace.as_mut());
    for thread in &mut event.threads.values {
        thread.id = None;
        thread.name = None;
        sanitize_stacktrace(thread.stacktrace.as_mut());
        sanitize_stacktrace(thread.raw_stacktrace.as_mut());
    }
    event
}

fn sanitize_stacktrace(stacktrace: Option<&mut Stacktrace>) {
    let Some(stacktrace) = stacktrace else {
        return;
    };
    for frame in &mut stacktrace.frames {
        frame.abs_path = None;
        frame.filename = frame
            .filename
            .as_deref()
            .and_then(|path| path.rsplit(['/', '\\']).next())
            .filter(|name| {
                name.ends_with(".rs")
                    && name.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
                    })
            })
            .map(str::to_string);
        frame.package = None;
        frame.pre_context.clear();
        frame.context_line = None;
        frame.post_context.clear();
        frame.vars.clear();
    }
    stacktrace.registers.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::protocol::{Exception, Frame, User, Values};

    #[test]
    fn enable_disable_enable_rebinds_a_new_client() {
        let state = TelemetryState::default();
        state.set_enabled_with_options(true, || client_options(None));
        assert!(state.enabled());
        assert_eq!(state.generation(), 1);
        state.set_enabled_with_options(false, || unreachable!());
        assert!(!state.enabled());
        state.set_enabled_with_options(true, || client_options(None));
        assert!(state.enabled());
        assert_eq!(state.generation(), 2);
        state.set_enabled_with_options(false, || unreachable!());
    }

    #[test]
    fn telemetry_options_disable_non_failure_channels() {
        let options = client_options(None);
        assert!(!options.auto_session_tracking);
        assert!(!options.enable_logs);
        assert!(!options.enable_metrics);
    }

    #[test]
    fn remote_event_drops_paths_profiles_archives_and_users() {
        let mut event = Event {
            message: Some(r"C:\Users\Alice\AppData\Temp\secret-archive.zip 123/1-S2-1-456".into()),
            user: Some(User {
                username: Some("Alice".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        event.exception = Values::from(vec![Exception {
            value: Some(r"failed at C:\Users\Alice\secret.zip".into()),
            stacktrace: Some(Stacktrace {
                frames: vec![Frame {
                    filename: Some(r"C:\Users\Alice\src\workflow.rs".into()),
                    abs_path: Some(r"C:\Users\Alice\src\workflow.rs".into()),
                    pre_context: vec![r#"let profile = "Alice";"#.into()],
                    context_line: Some(r#"open("C:\Users\Alice\secret.zip")"#.into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }]);
        let json = serde_json::to_string(&sanitize_event(event)).unwrap();
        for secret in ["Alice", "secret-archive", "1-S2-1", "AppData", "Temp"] {
            assert!(!json.contains(secret), "event leaked {secret}: {json}");
        }
        assert!(json.contains("workflow.rs"));
    }

    #[test]
    fn sentry_transport_receives_only_sanitized_payload_and_safe_tags() {
        let events = sentry::test::with_captured_events_options(
            || {
                sentry::with_scope(
                    |scope| {
                        scope.set_tag("operation", "activate");
                        scope.set_tag("error_code", "ledger_invariant");
                        scope.set_tag("archive", "private-campaign.zip");
                        scope.set_user(Some(User {
                            username: Some("Alice".into()),
                            ..Default::default()
                        }));
                    },
                    || {
                        sentry::capture_message(
                            r"C:\Users\Alice\AppData\Temp\private-campaign.zip",
                            sentry::Level::Error,
                        );
                    },
                );
            },
            client_options(None),
        );
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("ledger_invariant"));
        assert!(json.contains("activate"));
        for secret in ["Alice", "private-campaign", "AppData", "archive"] {
            assert!(!json.contains(secret), "transport leaked {secret}: {json}");
        }
    }

    #[test]
    fn non_internal_errors_are_never_captured() {
        let state = TelemetryState::default();
        state.set_enabled_with_options(true, || client_options(None));
        let user = svccm_core::error::user_err("locked_file", "close the file");
        let package = svccm_core::error::package_err("bad_archive", "invalid archive");
        let environment =
            svccm_core::Error::Environment(svccm_core::error::EnvironmentError::GameNotFound);
        let calls = std::cell::Cell::new(0);
        for error in [&user, &package, &environment] {
            assert!(capture_internal_with(&state, "test", error, |_, _| {
                calls.set(calls.get() + 1);
                "unexpected".into()
            })
            .is_none());
        }
        assert_eq!(calls.get(), 0);
        let internal =
            svccm_core::error::internal_err("ledger_invariant", "operation failed", "local detail");
        assert_eq!(
            capture_internal_with(&state, "activate package", &internal, |operation, code| {
                calls.set(calls.get() + 1);
                format!("{operation}:{code}")
            })
            .as_deref(),
            Some("activatepackage:ledger_invariant")
        );
        assert_eq!(calls.get(), 1);
        state.set_enabled_with_options(false, || unreachable!());
    }

    #[test]
    fn disabled_telemetry_never_sends_internal_errors() {
        let state = TelemetryState::default();
        let internal =
            svccm_core::error::internal_err("ledger_invariant", "operation failed", "local detail");
        let calls = std::cell::Cell::new(0);

        assert!(
            capture_internal_with(&state, "activate", &internal, |_, _| {
                calls.set(calls.get() + 1);
                "unexpected".into()
            })
            .is_none()
        );
        assert_eq!(calls.get(), 0);
    }
}
