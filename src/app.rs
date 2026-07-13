use crate::mcp::{UiEvent, UiEventKind};
use eframe::egui;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const MAX_EVENTS: usize = 500;

fn retain_recent_events(events: &mut Vec<UiEvent>) {
    if events.len() > MAX_EVENTS {
        events.drain(0..events.len() - MAX_EVENTS);
    }
}

fn event_description(kind: &UiEventKind) -> String {
    match kind {
        UiEventKind::WorkerStarted => "Background worker started".to_string(),
        UiEventKind::ServerStarting => "Stdio MCP server starting".to_string(),
        UiEventKind::WaitingForClient => "Waiting for MCP client initialisation".to_string(),
        UiEventKind::ClientConnected => "MCP client successfully initialised".to_string(),
        UiEventKind::PingRequested => "Tool 'ping' requested by client".to_string(),
        UiEventKind::PingResponded => "Tool 'ping' responded with 'pong'".to_string(),
        UiEventKind::LaunchProcessRequested { process_name } => {
            format!("Tool 'launch_process' requested for '{process_name}'")
        }
        UiEventKind::LaunchProcessResponded { status, pid } => {
            if let Some(pid) = pid {
                format!("Tool 'launch_process' responded: {status:?} (PID {pid})")
            } else {
                format!("Tool 'launch_process' responded: {status:?}")
            }
        }
        UiEventKind::LaunchProcessRejected { error } => {
            format!("Tool 'launch_process' rejected: {error}")
        }
        UiEventKind::LaunchProcessBackgroundError { pid, error } => {
            format!("Background process handling failed for PID {pid}: {error}")
        }
        UiEventKind::ReadFileRequested {
            path,
            start_line,
            end_line,
        } => format!("Tool 'read_file' requested '{path}' lines {start_line}-{end_line}"),
        UiEventKind::ReadFileResponded {
            status,
            actual_start_line,
            actual_end_line,
        } => match (actual_start_line, actual_end_line) {
            (Some(start), Some(end)) => {
                format!("Tool 'read_file' responded: {status:?} (lines {start}-{end})")
            }
            _ => format!("Tool 'read_file' responded: {status:?} (no lines returned)"),
        },
        UiEventKind::ReadFileRejected { error } => {
            format!("Tool 'read_file' rejected: {error}")
        }
        UiEventKind::ServerStopped => "MCP service stopped".to_string(),
        UiEventKind::ServerError { error } => format!("Fatal MCP error: {error}"),
    }
}

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

        retain_recent_events(&mut self.events);

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
                        let description = event_description(&event.kind);
                        ui.label(format!("[{:>07.3}s] {}", secs, description));
                    }
                });
        });

        // Request repaint after an interval around 100-250ms to check for new events
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::ReadFileStatus;

    #[test]
    fn background_error_event_is_formatted_for_the_gui() {
        let description = event_description(&UiEventKind::LaunchProcessBackgroundError {
            pid: 52,
            error: "Detached reaper failed: injected wait failure".to_string(),
        });

        assert_eq!(
            description,
            "Background process handling failed for PID 52: Detached reaper failed: injected wait failure"
        );
        assert_eq!(description.matches("PID 52").count(), 1);
        assert!(description.contains("Detached reaper failed"));
        assert!(description.contains("injected wait failure"));
    }

    #[test]
    fn events_are_capped_and_displayed_newest_first() {
        let mut events = (0..=MAX_EVENTS)
            .map(|index| UiEvent {
                elapsed: Duration::from_secs(index as u64),
                kind: UiEventKind::PingRequested,
            })
            .collect::<Vec<_>>();

        retain_recent_events(&mut events);

        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events.first().unwrap().elapsed, Duration::from_secs(1));
        assert_eq!(
            events.iter().next_back().unwrap().elapsed,
            Duration::from_secs(MAX_EVENTS as u64)
        );
    }

    #[test]
    fn read_file_events_are_concise_and_never_include_file_text() {
        let requested = event_description(&UiEventKind::ReadFileRequested {
            path: r"T:\Temp\example.rs".to_string(),
            start_line: 20,
            end_line: 40,
        });
        let responded = event_description(&UiEventKind::ReadFileResponded {
            status: ReadFileStatus::Completed,
            actual_start_line: Some(20),
            actual_end_line: Some(31),
        });
        let rejected = event_description(&UiEventKind::ReadFileRejected {
            error: "start_line must be at least 1".to_string(),
        });

        assert_eq!(
            requested,
            r"Tool 'read_file' requested 'T:\Temp\example.rs' lines 20-40"
        );
        assert_eq!(
            responded,
            "Tool 'read_file' responded: Completed (lines 20-31)"
        );
        assert_eq!(
            rejected,
            "Tool 'read_file' rejected: start_line must be at least 1"
        );
        for description in [requested, responded, rejected] {
            assert!(!description.contains("private file body"));
        }
    }
}
