#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod mcp;
mod tunnel;

use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn main() -> eframe::Result {
    let start_time = Instant::now();
    let app = if tunnel::has_mcp_stdio_transport() {
        let (tx, rx) = mpsc::channel();

        // Spawn named background thread for MCP worker only when an MCP host
        // supplied the required stdin/stdout pipes.
        thread::Builder::new()
            .name("mcp_worker".to_string())
            .spawn(move || {
                mcp::run_mcp_server(tx, start_time);
            })
            .expect("Failed to spawn background MCP worker thread");

        app::RemoteControlApp::new(rx, start_time)
    } else {
        app::RemoteControlApp::new_standalone(start_time)
    };

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
        .expect("embedded application icon should be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "Remote Control MCP",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
}
