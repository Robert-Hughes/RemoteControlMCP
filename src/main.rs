#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod mcp;

use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn main() -> eframe::Result {
    let start_time = Instant::now();
    let (tx, rx) = mpsc::channel();

    // Spawn named background thread for MCP worker
    thread::Builder::new()
        .name("mcp_worker".to_string())
        .spawn(move || {
            mcp::run_mcp_server(tx, start_time);
        })
        .expect("Failed to spawn background MCP worker thread");

    let options = eframe::NativeOptions::default();

    eframe::run_native(
        "Remote Control MCP",
        options,
        Box::new(move |_cc| Ok(Box::new(app::RemoteControlApp::new(rx, start_time)))),
    )
}
