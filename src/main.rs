#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod disk_log;
mod mcp;
mod tunnel;

use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn main() -> eframe::Result {
    let start_time = Instant::now();
    let (tx, rx) = mpsc::channel();

    // The GUI stays on the main thread while the loopback HTTP MCP server owns
    // its single-threaded Tokio runtime on this dedicated worker thread.
    thread::Builder::new()
        .name("mcp_worker".to_string())
        .spawn(move || {
            mcp::run_mcp_server(tx, start_time);
        })
        .expect("Failed to spawn background MCP worker thread");

    let app = app::RemoteControlApp::new(rx, start_time);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_icon(app::normal_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Remote Control MCP",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
}
