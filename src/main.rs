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

    #[cfg(windows)]
    let mut wgpu_options = eframe::WgpuConfiguration::default();
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(ref mut setup) = wgpu_options.wgpu_setup {
        // NVFlash can temporarily disconnect/reset the NVIDIA GPU while reading firmware.
        // The glow/OpenGL renderer dies with the driver in that situation, and eframe's
        // default wgpu setup also permits GL as a fallback. Force DX12 only on Windows so
        // the UI never creates an OpenGL context; Unix/GhostBSD keeps its existing glow path.
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_icon(app::normal_icon()),
        #[cfg(windows)]
        renderer: eframe::Renderer::Wgpu,
        #[cfg(windows)]
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "Remote Control MCP",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
}
