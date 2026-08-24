//! Anonymized usage analytics (Aptabase, EU region).
//!
//! Opt-out model: enabled by default, disabled from Settings or the
//! first-launch disclaimer. The plugin sends nothing on its own - every
//! event goes through [`track`], which no-ops while disabled.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::AppHandle;
use tauri_plugin_aptabase::{EventTracker, InitOptions};

/// Public app key - a routing identifier, not a credential.
const KEY: &str = "A-EU-3510622030";
const HOST: &str = "https://eu.appinsights.aptabase.com";

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_aptabase::Builder::new(KEY)
        .with_options(InitOptions {
            host: Some(HOST.into()),
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
