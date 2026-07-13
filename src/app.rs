use crate::mcp::{
    LaunchProcessStatus, ReadFileStatus, RequestData, RequestId, RequestUpdate, UiEvent,
    UiEventKind, WriteFileStatus,
};
use chrono::{DateTime, Local, TimeZone};
use eframe::egui;
use std::fmt::Display;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const MAX_REQUESTS: usize = 500;
const MAX_COMMAND_LINE_CHARACTERS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestState {
    InProgress,
    Completed,
    Warning,
    Failed,
    Rejected,
}

#[derive(Debug, Clone)]
struct RequestEntry {
    id: RequestId,
    request: RequestData,
    started_at: DateTime<Local>,
    started_elapsed: Duration,
    finished_duration: Option<Duration>,
    state: RequestState,
    status_text: String,
    detail_text: Option<String>,
    pid: Option<u32>,
    background_failure: bool,
}

impl RequestEntry {
    fn duration(&self, current_elapsed: Duration) -> Duration {
        self.finished_duration
            .unwrap_or_else(|| current_elapsed.saturating_sub(self.started_elapsed))
    }
}

struct RequestPresentation {
    state: RequestState,
    status_text: String,
    detail_text: Option<String>,
    pid: Option<u32>,
}

fn launch_process_presentation(
    status: LaunchProcessStatus,
    error: Option<String>,
    pid: Option<u32>,
    exit_code: Option<i32>,
) -> RequestPresentation {
    let (state, status_text) = match status {
        LaunchProcessStatus::Completed => match exit_code {
            Some(0) => (
                RequestState::Completed,
                "Completed · exit code 0".to_string(),
            ),
            Some(code) => (
                RequestState::Warning,
                format!("Completed · exit code {code}"),
            ),
            None => (RequestState::Completed, "Completed".to_string()),
        },
        LaunchProcessStatus::Detached => (RequestState::Warning, "Detached".to_string()),
        LaunchProcessStatus::DetachedWithStopTimeout => (
            RequestState::Warning,
            "Detached · stop timeout active".to_string(),
        ),
        LaunchProcessStatus::TimedOutDetached => {
            (RequestState::Warning, "Timed out · detached".to_string())
        }
        LaunchProcessStatus::TimedOutStopped => {
            (RequestState::Warning, "Timed out · stopped".to_string())
        }
        LaunchProcessStatus::SetupFailed => (RequestState::Failed, "Setup failed".to_string()),
        LaunchProcessStatus::LaunchProcessFailed => {
            (RequestState::Failed, "Launch failed".to_string())
        }
        LaunchProcessStatus::WaitFailed => (RequestState::Failed, "Wait failed".to_string()),
        LaunchProcessStatus::StopFailed => (RequestState::Failed, "Stop failed".to_string()),
    };
    RequestPresentation {
        state,
        status_text,
        detail_text: (state == RequestState::Failed).then_some(error).flatten(),
        pid,
    }
}

fn read_file_presentation(
    status: ReadFileStatus,
    error: Option<String>,
    actual_start_line: Option<u64>,
    actual_end_line: Option<u64>,
    next_start_line: Option<u64>,
    eof: Option<bool>,
) -> RequestPresentation {
    let (state, status_text) = match status {
        ReadFileStatus::Completed => match (actual_start_line, actual_end_line) {
            (Some(start), Some(end)) => {
                let suffix = if eof == Some(true) {
                    " · end of file reached"
                } else {
                    ""
                };
                (
                    RequestState::Completed,
                    format!("Completed · lines {start}–{end}{suffix}"),
                )
            }
            _ => (
                RequestState::Completed,
                "Completed · no lines returned".to_string(),
            ),
        },
        ReadFileStatus::Truncated => (
            RequestState::Warning,
            next_start_line.map_or_else(
                || "Truncated".to_string(),
                |line| format!("Truncated · continue from line {line}"),
            ),
        ),
        ReadFileStatus::NotFound => (RequestState::Failed, "File not found".to_string()),
        ReadFileStatus::AccessDenied => (RequestState::Failed, "Access denied".to_string()),
        ReadFileStatus::NotAFile => (RequestState::Failed, "Not a regular file".to_string()),
        ReadFileStatus::ReadFailed => (RequestState::Failed, "Read failed".to_string()),
        ReadFileStatus::LineTooLong => (RequestState::Failed, "Line exceeds 256 KiB".to_string()),
    };
    RequestPresentation {
        state,
        status_text,
        detail_text: (state == RequestState::Failed).then_some(error).flatten(),
        pid: None,
    }
}

fn write_file_presentation(
    status: WriteFileStatus,
    error: Option<String>,
    replaced_line_count: Option<u64>,
    inserted_bytes: u64,
) -> RequestPresentation {
    let (state, status_text) = match status {
        WriteFileStatus::Completed if inserted_bytes == 0 => (
            RequestState::Completed,
            replaced_line_count.map_or_else(
                || "Completed \u{00b7} lines deleted".to_string(),
                |count| format!("Completed \u{00b7} deleted {count} lines"),
            ),
        ),
        WriteFileStatus::Completed => (
            RequestState::Completed,
            replaced_line_count.map_or_else(
                || "Completed \u{00b7} lines replaced".to_string(),
                |count| format!("Completed \u{00b7} replaced {count} lines"),
            ),
        ),
        WriteFileStatus::Created => (
            RequestState::Completed,
            format!("Created \u{00b7} {inserted_bytes} bytes"),
        ),
        WriteFileStatus::NotFound => (RequestState::Failed, "File not found".to_string()),
        WriteFileStatus::ParentNotFound => (
            RequestState::Failed,
            "Parent directory not found".to_string(),
        ),
        WriteFileStatus::ParentNotADirectory => (
            RequestState::Failed,
            "Parent is not a directory".to_string(),
        ),
        WriteFileStatus::AccessDenied => (RequestState::Failed, "Access denied".to_string()),
        WriteFileStatus::NotAFile => (RequestState::Failed, "Not a regular file".to_string()),
        WriteFileStatus::RangeOutOfBounds => {
            (RequestState::Failed, "Line range out of bounds".to_string())
        }
        WriteFileStatus::ReadFailed => (RequestState::Failed, "Read failed".to_string()),
        WriteFileStatus::WriteFailed => (RequestState::Failed, "Write failed".to_string()),
        WriteFileStatus::ReplaceFailed => (
            RequestState::Failed,
            "Replacement commit failed".to_string(),
        ),
    };
    RequestPresentation {
        state,
        status_text,
        detail_text: (state == RequestState::Failed).then_some(error).flatten(),
        pid: None,
    }
}

fn presentation_for_update(update: RequestUpdate) -> RequestPresentation {
    match update {
        RequestUpdate::PingCompleted => RequestPresentation {
            state: RequestState::Completed,
            status_text: "Completed".to_string(),
            detail_text: None,
            pid: None,
        },
        RequestUpdate::LaunchProcessResponded {
            status,
            error,
            pid,
            exit_code,
        } => launch_process_presentation(status, error, pid, exit_code),
        RequestUpdate::ReadFileResponded {
            status,
            error,
            actual_start_line,
            actual_end_line,
            next_start_line,
            eof,
        } => read_file_presentation(
            status,
            error,
            actual_start_line,
            actual_end_line,
            next_start_line,
            eof,
        ),
        RequestUpdate::WriteFileResponded {
            status,
            error,
            replaced_line_count,
            inserted_bytes,
        } => write_file_presentation(status, error, replaced_line_count, inserted_bytes),
        RequestUpdate::Rejected { error } => RequestPresentation {
            state: RequestState::Rejected,
            status_text: "Invalid parameters".to_string(),
            detail_text: Some(error),
            pid: None,
        },
        RequestUpdate::InternalFailure { error } => RequestPresentation {
            state: RequestState::Failed,
            status_text: "Response construction failed".to_string(),
            detail_text: Some(error),
            pid: None,
        },
        RequestUpdate::LaunchProcessBackgroundError { pid, error } => RequestPresentation {
            state: RequestState::Failed,
            status_text: "Background process handling failed".to_string(),
            detail_text: Some(error),
            pid: Some(pid),
        },
    }
}

fn prune_requests(requests: &mut Vec<RequestEntry>) {
    while requests.len() > MAX_REQUESTS {
        let Some(index) = requests
            .iter()
            .position(|request| request.state != RequestState::InProgress)
        else {
            break;
        };
        requests.remove(index);
    }
}

fn apply_request_event(requests: &mut Vec<RequestEntry>, event: UiEvent) {
    match event.kind {
        UiEventKind::RequestStarted {
            id,
            request,
            started_at,
        } => requests.push(RequestEntry {
            id,
            request,
            started_at,
            started_elapsed: event.elapsed,
            finished_duration: None,
            state: RequestState::InProgress,
            status_text: "In progress".to_string(),
            detail_text: None,
            pid: None,
            background_failure: false,
        }),
        UiEventKind::RequestUpdated { id, update } => {
            if let Some(request) = requests.iter_mut().rev().find(|request| request.id == id) {
                let is_primary_terminal = matches!(
                    &update,
                    RequestUpdate::PingCompleted
                        | RequestUpdate::LaunchProcessResponded { .. }
                        | RequestUpdate::ReadFileResponded { .. }
                        | RequestUpdate::WriteFileResponded { .. }
                        | RequestUpdate::Rejected { .. }
                        | RequestUpdate::InternalFailure { .. }
                );
                let is_background_failure =
                    matches!(&update, RequestUpdate::LaunchProcessBackgroundError { .. });
                if is_primary_terminal && request.finished_duration.is_none() {
                    request.finished_duration =
                        Some(event.elapsed.saturating_sub(request.started_elapsed));
                }

                let presentation = presentation_for_update(update);
                if is_background_failure {
                    request.background_failure = true;
                }

                if is_background_failure || !request.background_failure {
                    request.state = presentation.state;
                    request.status_text = presentation.status_text;
                    request.detail_text = presentation.detail_text;
                }
                if presentation.pid.is_some() {
                    request.pid = presentation.pid;
                }
            }
        }
        _ => return,
    }
    prune_requests(requests);
}

fn format_start_time<Tz>(started_at: &DateTime<Tz>) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    started_at.format("%d/%m/%Y %H:%M:%S").to_string()
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{seconds:.3}s")
    } else if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let minutes = duration.as_secs() / 60;
        let seconds = duration.as_secs() % 60;
        format!("{minutes}m {seconds:02}s")
    }
}

fn request_tool_name(request: &RequestData) -> &'static str {
    match request {
        RequestData::Ping => "ping",
        RequestData::LaunchProcess { .. } => "launch_process",
        RequestData::ReadFile { .. } => "read_file",
        RequestData::WriteFile { .. } => "write_file",
    }
}

fn request_summary(request: &RequestEntry) -> String {
    match &request.request {
        RequestData::Ping => "Server health check".to_string(),
        RequestData::LaunchProcess { command_line } => {
            let command_line = truncate_with_ellipsis(command_line, MAX_COMMAND_LINE_CHARACTERS);
            request.pid.map_or(command_line.clone(), |pid| {
                format!("{command_line} · PID {pid}")
            })
        }
        RequestData::ReadFile {
            path,
            start_line,
            end_line,
        } => format!("{path} · requested lines {start_line}–{end_line}"),
        RequestData::WriteFile {
            path,
            start_line,
            end_line,
            replacement_bytes,
            create_if_missing,
        } => {
            let create_suffix = if *create_if_missing {
                " \u{00b7} create if missing"
            } else {
                ""
            };
            format!(
                "{path} \u{00b7} requested lines {start_line}\u{2013}{end_line} \u{00b7} {replacement_bytes}-byte replacement{create_suffix}"
            )
        }
    }
}

fn truncate_with_ellipsis(text: &str, maximum_characters: usize) -> String {
    let character_count = text.chars().count();
    if character_count <= maximum_characters {
        return text.to_string();
    }

    text.chars()
        .take(maximum_characters.saturating_sub(1))
        .chain((maximum_characters != 0).then_some('…'))
        .collect()
}

fn request_summary_tooltip(request: &RequestEntry) -> Option<&str> {
    match &request.request {
        RequestData::LaunchProcess { command_line } => Some(command_line),
        RequestData::Ping | RequestData::ReadFile { .. } | RequestData::WriteFile { .. } => None,
    }
}

fn paint_state_icon(ui: &mut egui::Ui, state: RequestState, colour: egui::Color32) {
    let (response, painter) = ui.allocate_painter(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let rect = response.rect.shrink(2.0);
    let stroke = egui::Stroke::new(2.0, colour);

    match state {
        RequestState::Completed => {
            let middle = egui::pos2(rect.left() + rect.width() * 0.4, rect.bottom());
            painter.line_segment([egui::pos2(rect.left(), rect.center().y), middle], stroke);
            painter.line_segment([middle, egui::pos2(rect.right(), rect.top())], stroke);
        }
        RequestState::Warning => {
            let top = egui::pos2(rect.center().x, rect.top());
            let left = egui::pos2(rect.left(), rect.bottom());
            let right = egui::pos2(rect.right(), rect.bottom());
            painter.line_segment([top, left], stroke);
            painter.line_segment([left, right], stroke);
            painter.line_segment([right, top], stroke);
            painter.line_segment(
                [
                    egui::pos2(rect.center().x, rect.top() + 3.5),
                    egui::pos2(rect.center().x, rect.bottom() - 4.0),
                ],
                stroke,
            );
            painter.circle_filled(
                egui::pos2(rect.center().x, rect.bottom() - 1.5),
                1.0,
                colour,
            );
        }
        RequestState::Failed => {
            painter.line_segment([rect.left_top(), rect.right_bottom()], stroke);
            painter.line_segment([rect.right_top(), rect.left_bottom()], stroke);
        }
        RequestState::Rejected => {
            painter.circle_stroke(rect.center(), rect.width() / 2.0, stroke);
            painter.line_segment([rect.left_bottom(), rect.right_top()], stroke);
        }
        RequestState::InProgress => {}
    }
}

fn state_colour(ui: &egui::Ui, state: RequestState) -> egui::Color32 {
    match state {
        RequestState::InProgress | RequestState::Completed => ui.visuals().strong_text_color(),
        RequestState::Warning | RequestState::Rejected => ui.visuals().warn_fg_color,
        RequestState::Failed => ui.visuals().error_fg_color,
    }
}

fn render_request_row(ui: &mut egui::Ui, request: &RequestEntry, current_elapsed: Duration) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(20.0, 20.0),
                egui::Layout::top_down(egui::Align::Center),
                |ui| match request.state {
                    RequestState::InProgress => {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                    RequestState::Completed => {
                        paint_state_icon(ui, request.state, state_colour(ui, request.state));
                    }
                    state => {
                        paint_state_icon(ui, state, state_colour(ui, state));
                    }
                },
            );
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(request_tool_name(&request.request));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(state_colour(ui, request.state), &request.status_text);
                    });
                });
                let summary = ui.label(request_summary(request));
                if let Some(command_line) = request_summary_tooltip(request) {
                    summary.on_hover_text(command_line);
                }
                ui.weak(format!(
                    "Started {} · Duration {}",
                    format_start_time(&request.started_at),
                    format_duration(request.duration(current_elapsed))
                ));
                if let Some(detail) = &request.detail_text {
                    ui.label(detail);
                }
            });
        });
    });
}

pub struct RemoteControlApp {
    rx: Receiver<UiEvent>,
    requests: Vec<RequestEntry>,
    status_text: String,
    fatal_error: Option<String>,
    start_time: Instant,
}

impl RemoteControlApp {
    pub fn new(rx: Receiver<UiEvent>, start_time: Instant) -> Self {
        Self {
            rx,
            requests: Vec::new(),
            status_text: "Starting".to_string(),
            fatal_error: None,
            start_time,
        }
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match &event.kind {
                UiEventKind::WorkerStarted => self.status_text = "Worker started".to_string(),
                UiEventKind::ServerStarting => self.status_text = "Server starting".to_string(),
                UiEventKind::WaitingForClient => {
                    self.status_text = "Waiting for MCP client".to_string();
                }
                UiEventKind::ClientConnected => self.status_text = "Connected".to_string(),
                UiEventKind::ServerStopped => self.status_text = "Stopped".to_string(),
                UiEventKind::ServerError { error } => {
                    self.status_text = "Error".to_string();
                    self.fatal_error = Some(error.clone());
                }
                UiEventKind::RequestStarted { .. } | UiEventKind::RequestUpdated { .. } => {
                    apply_request_event(&mut self.requests, event);
                }
            }
        }
    }
}

impl eframe::App for RemoteControlApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_events();
        let current_elapsed = self.start_time.elapsed();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Remote Control MCP");
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.label("Current Status:");
                ui.strong(&self.status_text);
            });
            if let Some(error) = &self.fatal_error {
                ui.add_space(5.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, "Fatal server error");
                    ui.label(error);
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(5.0);
            ui.label("Requests:");
            ui.add_space(5.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for request in self.requests.iter().rev() {
                        render_request_row(ui, request, current_elapsed);
                        ui.add_space(4.0);
                    }
                });
        });

        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn started_event(id: u64, elapsed: Duration) -> UiEvent {
        UiEvent {
            elapsed,
            kind: UiEventKind::RequestStarted {
                id: RequestId(id),
                request: RequestData::Ping,
                started_at: Local::now(),
            },
        }
    }

    fn updated_event(id: u64, elapsed: Duration, update: RequestUpdate) -> UiEvent {
        UiEvent {
            elapsed,
            kind: UiEventKind::RequestUpdated {
                id: RequestId(id),
                update,
            },
        }
    }

    #[test]
    fn start_time_format_is_deterministic_and_has_whole_seconds() {
        let time = FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2026, 7, 13, 18, 42, 7)
            .unwrap();
        let formatted = format_start_time(&time);
        assert_eq!(formatted, "13/07/2026 18:42:07");
        assert!(!formatted.contains('.'));
    }

    #[test]
    fn durations_are_compact_and_deterministic() {
        assert_eq!(format_duration(Duration::from_millis(321)), "0.321s");
        assert_eq!(format_duration(Duration::from_millis(2_100)), "2.1s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn requests_update_in_place_without_reordering_and_duration_freezes() {
        let mut requests = Vec::new();
        apply_request_event(&mut requests, started_event(1, Duration::from_secs(2)));
        let started_at = requests[0].started_at;
        apply_request_event(&mut requests, started_event(2, Duration::from_secs(3)));
        apply_request_event(
            &mut requests,
            updated_event(1, Duration::from_secs(5), RequestUpdate::PingCompleted),
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].id, RequestId(1));
        assert_eq!(requests[0].started_at, started_at);
        assert_eq!(
            requests[0].duration(Duration::from_secs(20)),
            Duration::from_secs(3)
        );
        assert_eq!(requests[1].state, RequestState::InProgress);

        apply_request_event(
            &mut requests,
            updated_event(
                1,
                Duration::from_secs(12),
                RequestUpdate::LaunchProcessBackgroundError {
                    pid: 42,
                    error: "injected failure".to_string(),
                },
            ),
        );
        assert_eq!(requests[0].state, RequestState::Failed);
        assert_eq!(
            requests[0].duration(Duration::from_secs(20)),
            Duration::from_secs(3)
        );

        apply_request_event(
            &mut requests,
            updated_event(999, Duration::from_secs(13), RequestUpdate::PingCompleted),
        );
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn background_failure_before_launch_response_is_sticky() {
        let mut requests = Vec::new();
        apply_request_event(
            &mut requests,
            UiEvent {
                elapsed: Duration::from_secs(2),
                kind: UiEventKind::RequestStarted {
                    id: RequestId(1),
                    request: RequestData::LaunchProcess {
                        command_line: "test.exe --background".to_string(),
                    },
                    started_at: Local::now(),
                },
            },
        );
        apply_request_event(
            &mut requests,
            updated_event(
                1,
                Duration::from_secs(3),
                RequestUpdate::LaunchProcessBackgroundError {
                    pid: 42,
                    error: "injected wait failure".to_string(),
                },
            ),
        );

        assert_eq!(requests[0].state, RequestState::Failed);
        assert_eq!(
            requests[0].status_text,
            "Background process handling failed"
        );
        assert_eq!(
            requests[0].detail_text.as_deref(),
            Some("injected wait failure")
        );
        assert!(requests[0].finished_duration.is_none());

        apply_request_event(
            &mut requests,
            updated_event(
                1,
                Duration::from_secs(7),
                RequestUpdate::LaunchProcessResponded {
                    status: LaunchProcessStatus::Detached,
                    error: None,
                    pid: Some(42),
                    exit_code: None,
                },
            ),
        );

        assert_eq!(requests[0].state, RequestState::Failed);
        assert_eq!(
            requests[0].status_text,
            "Background process handling failed"
        );
        assert_eq!(
            requests[0].detail_text.as_deref(),
            Some("injected wait failure")
        );
        assert_eq!(requests[0].pid, Some(42));
        assert_eq!(
            requests[0].duration(Duration::from_secs(20)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn in_progress_duration_uses_current_monotonic_elapsed_time() {
        let mut requests = Vec::new();
        apply_request_event(&mut requests, started_event(1, Duration::from_secs(2)));
        assert_eq!(
            requests[0].duration(Duration::from_secs(7)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn retention_removes_oldest_finished_and_preserves_active_requests() {
        let mut requests = Vec::new();
        for id in 1..=(MAX_REQUESTS as u64 + 1) {
            apply_request_event(&mut requests, started_event(id, Duration::from_secs(id)));
        }
        assert_eq!(requests.len(), MAX_REQUESTS + 1);

        apply_request_event(
            &mut requests,
            updated_event(1, Duration::from_secs(700), RequestUpdate::PingCompleted),
        );
        assert_eq!(requests.len(), MAX_REQUESTS);
        assert!(!requests.iter().any(|request| request.id == RequestId(1)));
        assert!(
            requests
                .iter()
                .all(|request| request.state == RequestState::InProgress)
        );

        apply_request_event(&mut requests, started_event(700, Duration::from_secs(701)));
        apply_request_event(
            &mut requests,
            updated_event(2, Duration::from_secs(702), RequestUpdate::PingCompleted),
        );
        assert_eq!(requests.len(), MAX_REQUESTS);
        assert!(!requests.iter().any(|request| request.id == RequestId(2)));
    }

    #[test]
    fn finished_requests_are_capped_and_oldest_is_removed_first() {
        let mut requests = Vec::new();
        for id in 1..=(MAX_REQUESTS as u64 + 1) {
            apply_request_event(&mut requests, started_event(id, Duration::from_secs(id)));
            apply_request_event(
                &mut requests,
                updated_event(
                    id,
                    Duration::from_secs(id + 1),
                    RequestUpdate::PingCompleted,
                ),
            );
        }
        assert_eq!(requests.len(), MAX_REQUESTS);
        assert_eq!(requests.first().unwrap().id, RequestId(2));
        assert_eq!(requests.last().unwrap().id, RequestId(501));
        assert!(
            requests
                .iter()
                .all(|request| request.state == RequestState::Completed)
        );
    }

    #[test]
    fn every_tool_status_maps_to_the_required_state() {
        let completed =
            launch_process_presentation(LaunchProcessStatus::Completed, None, None, Some(0));
        let nonzero =
            launch_process_presentation(LaunchProcessStatus::Completed, None, None, Some(7));
        assert_eq!(completed.state, RequestState::Completed);
        assert_eq!(nonzero.state, RequestState::Warning);
        for status in [
            LaunchProcessStatus::Detached,
            LaunchProcessStatus::DetachedWithStopTimeout,
            LaunchProcessStatus::TimedOutDetached,
            LaunchProcessStatus::TimedOutStopped,
        ] {
            assert_eq!(
                launch_process_presentation(status, None, None, None).state,
                RequestState::Warning
            );
        }
        for status in [
            LaunchProcessStatus::SetupFailed,
            LaunchProcessStatus::LaunchProcessFailed,
            LaunchProcessStatus::WaitFailed,
            LaunchProcessStatus::StopFailed,
        ] {
            assert_eq!(
                launch_process_presentation(status, Some("error".to_string()), None, None).state,
                RequestState::Failed
            );
        }
        assert_eq!(
            presentation_for_update(RequestUpdate::PingCompleted).state,
            RequestState::Completed
        );
        assert_eq!(
            presentation_for_update(RequestUpdate::InternalFailure {
                error: "failure".to_string()
            })
            .state,
            RequestState::Failed
        );
        assert_eq!(
            presentation_for_update(RequestUpdate::Rejected {
                error: "invalid".to_string()
            })
            .state,
            RequestState::Rejected
        );

        assert_eq!(
            read_file_presentation(
                ReadFileStatus::Completed,
                None,
                None,
                None,
                None,
                Some(true),
            )
            .state,
            RequestState::Completed
        );
        assert_eq!(
            read_file_presentation(ReadFileStatus::Truncated, None, None, None, Some(2), None)
                .state,
            RequestState::Warning
        );
        for status in [
            ReadFileStatus::NotFound,
            ReadFileStatus::AccessDenied,
            ReadFileStatus::NotAFile,
            ReadFileStatus::ReadFailed,
            ReadFileStatus::LineTooLong,
        ] {
            assert_eq!(
                read_file_presentation(status, Some("error".to_string()), None, None, None, None)
                    .state,
                RequestState::Failed
            );
        }
    }

    #[test]
    fn launch_process_summary_includes_arguments_and_has_full_tooltip() {
        let launch = RequestEntry {
            id: RequestId(1),
            request: RequestData::LaunchProcess {
                command_line: "safe.exe visible argument".to_string(),
            },
            started_at: Local::now(),
            started_elapsed: Duration::ZERO,
            finished_duration: None,
            state: RequestState::InProgress,
            status_text: "In progress".to_string(),
            detail_text: None,
            pid: Some(42),
            background_failure: false,
        };
        assert_eq!(
            request_summary(&launch),
            "safe.exe visible argument · PID 42"
        );
        assert_eq!(
            request_summary_tooltip(&launch),
            Some("safe.exe visible argument")
        );
    }

    #[test]
    fn command_line_truncation_is_bounded_and_unicode_safe() {
        assert_eq!(truncate_with_ellipsis("abcdef", 6), "abcdef");
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "abcd…");
        assert_eq!(truncate_with_ellipsis("åßçdé", 4), "åßç…");
        assert_eq!(truncate_with_ellipsis("abcdef", 1), "…");
        assert_eq!(truncate_with_ellipsis("abcdef", 0), "");
    }

    #[test]
    fn write_file_statuses_summaries_and_terminal_updates_are_privacy_safe() {
        for status in [WriteFileStatus::Completed, WriteFileStatus::Created] {
            assert_eq!(
                write_file_presentation(status, None, Some(2), 12).state,
                RequestState::Completed
            );
        }
        for status in [
            WriteFileStatus::NotFound,
            WriteFileStatus::ParentNotFound,
            WriteFileStatus::ParentNotADirectory,
            WriteFileStatus::AccessDenied,
            WriteFileStatus::NotAFile,
            WriteFileStatus::RangeOutOfBounds,
            WriteFileStatus::ReadFailed,
            WriteFileStatus::WriteFailed,
            WriteFileStatus::ReplaceFailed,
        ] {
            assert_eq!(
                write_file_presentation(status, Some("safe detail".to_string()), None, 0).state,
                RequestState::Failed
            );
        }

        let request_data = RequestData::WriteFile {
            path: "C:\\safe\\file.txt".to_string(),
            start_line: 4,
            end_line: 6,
            replacement_bytes: 123,
            create_if_missing: true,
        };
        let mut requests = Vec::new();
        apply_request_event(
            &mut requests,
            UiEvent {
                elapsed: Duration::from_secs(2),
                kind: UiEventKind::RequestStarted {
                    id: RequestId(77),
                    request: request_data,
                    started_at: Local::now(),
                },
            },
        );
        assert_eq!(request_tool_name(&requests[0].request), "write_file");
        let summary = request_summary(&requests[0]);
        assert!(summary.contains("C:\\safe\\file.txt"));
        assert!(summary.contains("123-byte replacement"));
        assert!(summary.contains("create if missing"));

        apply_request_event(
            &mut requests,
            updated_event(
                77,
                Duration::from_secs(5),
                RequestUpdate::WriteFileResponded {
                    status: WriteFileStatus::Completed,
                    error: None,
                    replaced_line_count: Some(3),
                    inserted_bytes: 123,
                },
            ),
        );
        assert_eq!(requests[0].state, RequestState::Completed);
        assert_eq!(
            requests[0].duration(Duration::from_secs(20)),
            Duration::from_secs(3)
        );
        assert_eq!(requests[0].status_text, "Completed · replaced 3 lines");
        assert!(!format!("{:?}", requests[0].request).contains("replacement body"));
    }

    #[test]
    fn server_events_update_status_without_creating_requests() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = RemoteControlApp::new(rx, Instant::now());
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ClientConnected,
        })
        .unwrap();
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ServerError {
                error: "fatal detail".to_string(),
            },
        })
        .unwrap();
        app.receive_events();
        assert!(app.requests.is_empty());
        assert_eq!(app.status_text, "Error");
        assert_eq!(app.fatal_error.as_deref(), Some("fatal detail"));
    }
}
