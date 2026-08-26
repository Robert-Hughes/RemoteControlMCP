#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod disk_log;
mod mcp;
mod settings;
mod tunnel;
mod usage_log;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn main() -> eframe::Result {
    let start_time = Instant::now();
    let usage_log = usage_log::UsageLog::open();
    let (tx, rx) = mpsc::channel();
    let (maximum_request_timeout_seconds, maximum_request_timeout_setting_error) =
        match settings::load_maximum_request_timeout_seconds() {
            Ok(seconds) => (seconds, None),
            Err(error) => {
                eprintln!("{error}");
                (
                    settings::DEFAULT_MAXIMUM_REQUEST_TIMEOUT_SECONDS,
                    Some(error),
                )
            }
        };
    let maximum_request_timeout_seconds = Arc::new(AtomicU64::new(maximum_request_timeout_seconds));
    let mcp_maximum_request_timeout_seconds = Arc::clone(&maximum_request_timeout_seconds);

    // The GUI stays on the main thread while the loopback HTTP MCP server owns
    // its single-threaded Tokio runtime on this dedicated worker thread.
    thread::Builder::new()
        .name("mcp_worker".to_string())
        .spawn(move || {
            mcp::run_mcp_server(
                tx,
                start_time,
                mcp_maximum_request_timeout_seconds,
                usage_log,
            );
        })
        .expect("Failed to spawn background MCP worker thread");

    let app = app::RemoteControlApp::new(
        rx,
        start_time,
        maximum_request_timeout_seconds,
        maximum_request_timeout_setting_error,
    );

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
