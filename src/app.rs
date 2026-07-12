use crate::mcp::{UiEvent, UiEventKind};
use eframe::egui;
use std::sync::mpsc::Receiver;
use std::time::Duration;

pub struct RemoteControlApp {
    rx: Receiver<UiEvent>,
    events: Vec<UiEvent>,
    status_text: String,
}

impl RemoteControlApp {
    pub fn new(rx: Receiver<UiEvent>) -> Self {
        Self {
            rx,
            events: Vec::new(),
            status_text: "Starting".to_string(),
        }
    }
}

impl eframe::App for RemoteControlApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drain all events from the background thread
        while let Ok(event) = self.rx.try_recv() {
            match &event.kind {
                UiEventKind::WorkerStarted => {
                    self.status_text = "Worker started".to_string();
                }
                UiEventKind::ServerStarting => {
                    self.status_text = "Server starting".to_string();
                }
                UiEventKind::WaitingForClient => {
                    self.status_text = "Waiting for MCP client".to_string();
                }
                UiEventKind::ClientConnected => {
                    self.status_text = "Connected".to_string();
                }
                UiEventKind::ServerStopped => {
                    self.status_text = "Stopped".to_string();
                }
                UiEventKind::ServerError { error } => {
                    self.status_text = format!("Error: {}", error);
                }
                _ => {}
            }
            self.events.push(event);
        }

        // Keep at most 500 events
        if self.events.len() > 500 {
            let drain_count = self.events.len() - 500;
            self.events.drain(0..drain_count);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Remote Control MCP");
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.label("Current Status:");
                ui.strong(&self.status_text);
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(5.0);
            ui.label("Activity Log:");
            ui.add_space(5.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for event in self.events.iter().rev() {
                        let secs = event.elapsed.as_secs_f64();
                        let description = match &event.kind {
                            UiEventKind::WorkerStarted => "Background worker started".to_string(),
                            UiEventKind::ServerStarting => "Stdio MCP server starting".to_string(),
                            UiEventKind::WaitingForClient => {
                                "Waiting for MCP client initialisation".to_string()
                            }
                            UiEventKind::ClientConnected => {
                                "MCP client successfully initialised".to_string()
                            }
                            UiEventKind::PingRequested => {
                                "Tool 'ping' requested by client".to_string()
                            }
                            UiEventKind::PingResponded => {
                                "Tool 'ping' responded with 'pong'".to_string()
                            }
                            UiEventKind::LaunchProcessRequested { process_name } => {
                                format!("Tool 'launch_process' requested for '{}'", process_name)
                            }
                            UiEventKind::LaunchProcessResponded { status, pid } => {
                                if let Some(pid) = pid {
                                    format!(
                                        "Tool 'launch_process' responded: {:?} (PID {})",
                                        status, pid
                                    )
                                } else {
                                    format!("Tool 'launch_process' responded: {:?}", status)
                                }
                            }
                            UiEventKind::LaunchProcessRejected { error } => {
                                format!("Tool 'launch_process' rejected: {}", error)
                            }
                            UiEventKind::ServerStopped => "MCP service stopped".to_string(),
                            UiEventKind::ServerError { error } => {
                                format!("Fatal MCP error: {}", error)
                            }
                        };
                        ui.label(format!("[{:>07.3}s] {}", secs, description));
                    }
                });
        });

        // Request repaint after an interval around 100-250ms to check for new events
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}
