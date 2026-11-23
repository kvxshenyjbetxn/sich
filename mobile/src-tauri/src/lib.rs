//! Sich phone app core. Pure modules (`hosts`, `keys`, `backoff`, `ui`, `conn`) are testable on
//! the host; `commands` is the thin Tauri layer.

pub mod backoff;
pub mod chat;
pub mod commands;
pub mod conn;
pub mod hosts;
pub mod keys;
pub mod ui;

use std::sync::{Arc, Mutex};

use tauri::Manager;

use commands::{AppState, TauriUi};
use conn::{ConnConfig, ConnectionManager};
use hosts::HostStore;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    let app = builder
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let store = Arc::new(Mutex::new(HostStore::load(dir.join("hosts.json"))));
            let key = keys::load_or_create(&dir.join("phone.key"))?;
            let cfg = ConnConfig {
                relay_disabled: std::env::var("SICH_PHONE_RELAY").is_ok_and(|v| v == "disabled"),
                device_name: format!("Sich {}", std::env::consts::OS),
            };
            let ui = Arc::new(TauriUi { app: app.handle().clone() });
            let mgr = ConnectionManager::start(cfg, ui, store.clone(), key, tauri::async_runtime::handle().inner());
            let active = store.lock().unwrap().active().cloned();
            if let Some(host) = active {
                mgr.connect(host);
            }
            app.manage(AppState { mgr, store });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::hosts_list,
            commands::host_forget,
            commands::pair_start,
            commands::host_connect,
            commands::retry_now,
            commands::subscribe,
            commands::unsubscribe,
            commands::send_input,
            commands::send_text,
            commands::upload_image,
            commands::resize,
            commands::set_fit,
            commands::mark_seen,
            commands::connection_state,
            commands::workspace_snapshot,
            commands::gateway_capabilities,
            commands::chat_subscribe,
            commands::chat_unsubscribe,
            commands::chat_load_earlier,
            commands::chat_image,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Sich app");

    app.run(|_handle, _event| {
        #[cfg(mobile)]
        if let tauri::RunEvent::Resumed = _event {
            if let Some(state) = _handle.try_state::<AppState>() {
                state.mgr.resumed();
            }
        }
    });
}
