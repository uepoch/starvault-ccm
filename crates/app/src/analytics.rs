//! Anonymized usage analytics (Aptabase, EU region).
//!
//! Opt-out model: enabled by default, disabled from Settings or the
//! first-launch disclaimer. The plugin sends nothing on its own - every
//! event goes through [`track`], which no-ops while disabled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_aptabase::{EventTracker, InitOptions};

/// Public app key - a routing identifier, not a credential. The EU region
/// host is derived from the key (A-EU-...) by the plugin itself.
const KEY: &str = "A-EU-3510622030";
/// Events are tiny and rare; batch them and send twice an hour.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Budget granted to the exit flush before the app gives up (fail-open).
const EXIT_FLUSH_BUDGET: Duration = Duration::from_secs(1);

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_aptabase::Builder::new(KEY)
        .with_options(InitOptions {
            flush_interval: Some(FLUSH_INTERVAL),
            ..Default::default()
        })
        .build()
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Fire-and-forget event with string properties.
pub fn track(app: &AppHandle, event: &str, props: &[(&str, String)]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let props = serde_json::Map::from_iter(
        props
            .iter()
            .map(|(key, value)| ((*key).to_string(), serde_json::Value::from(value.clone()))),
    );
    let _ = app.track_event(event, Some(serde_json::Value::Object(props)));
}

/// Drain the queue at exit with a hard budget: the flush runs on a detached
/// thread and the app stops waiting after [`EXIT_FLUSH_BUDGET`] - events are
/// abandoned rather than delaying shutdown. Draining here also empties the
/// queue before the plugin's own RunEvent::Exit hook, whose blocking flush
/// would otherwise wait out the full HTTP timeout.
pub fn flush_on_exit(app: &AppHandle) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let app = app.clone();
    let (sent, done) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        app.flush_events_blocking();
        let _ = sent.send(());
    });
    let _ = done.recv_timeout(EXIT_FLUSH_BUDGET);
}
