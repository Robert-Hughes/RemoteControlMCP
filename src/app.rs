use crate::disk_log::DiskLog;
use crate::mcp::{
    LaunchProcessStatus, LocalInstructionsDiagnostic, ReadBinaryFileContentKind,
    ReadBinaryFileStatus, ReadFileStatus, RequestData, RequestId, RequestUpdate, UiEvent,
    UiEventKind, WriteFileStatus,
};
use crate::settings;
use crate::tunnel::{self, TunnelLaunch, TunnelLaunchEvent};
use chrono::{DateTime, Local, TimeZone};
use eframe::egui;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const MAX_RECENT_REQUESTS: usize = 100;
const MAX_COMMAND_LINE_CHARACTERS: usize = 80;
const BUSY_ICON_COOLDOWN: Duration = Duration::from_secs(30);

pub fn normal_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        Arc::new(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
                .expect("embedded application icon should be a valid PNG"),
        )
    }))
}

fn busy_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        Arc::new(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon-busy.png"))
                .expect("embedded busy application icon should be a valid PNG"),
        )
    }))
}

#[cfg(target_os = "macos")]
fn set_macos_application_icon(icon: &egui::IconData) {
    use objc2::{AnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage};
    use objc2_foundation::NSSize;

    let Some(main_thread_marker) = MainThreadMarker::new() else {
        eprintln!("cannot update macOS application icon off the main thread");
        return;
    };

    let mut planes = [icon.rgba.as_ptr().cast_mut()];
    // Match eframe's native macOS app-icon path: build NSImage from raw RGBA data
    // rather than decoding the embedded PNG again.
    let Some(image_rep) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            planes.as_mut_ptr(),
            icon.width as isize,
            icon.height as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (icon.width * 4) as isize,
            32,
        )
    }) else {
        eprintln!("failed to create macOS application icon bitmap");
        return;
    };

    let application_icon = NSImage::initWithSize(
        NSImage::alloc(),
        NSSize::new(icon.width as f64, icon.height as f64),
    );
    application_icon.addRepresentation(&image_rep);

    let application = NSApplication::sharedApplication(main_thread_marker);
    // SAFETY: the GUI runs on the macOS main thread and both Objective-C objects
    // remain alive for the duration of this call.
    unsafe {
        application.setApplicationIconImage(Some(&application_icon));
    }
}

struct AppIcons {
    normal: Arc<egui::IconData>,
    busy: Arc<egui::IconData>,
}

impl AppIcons {
    fn load() -> Self {
        Self {
            normal: normal_icon(),
            busy: busy_icon(),
        }
    }
}

#[derive(Debug)]
enum TunnelUiState {
    Idle,
    Starting { log_path: String },
    Running { log_path: String },
    Failed { error: String },
}

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
    stdout_file: Option<String>,
    stderr_file: Option<String>,
    stdout_lines: u64,
    stderr_lines: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
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

fn read_binary_file_presentation(
    status: ReadBinaryFileStatus,
    error: Option<String>,
    size: Option<u64>,
    mime_type: Option<String>,
    content_kind: Option<ReadBinaryFileContentKind>,
) -> RequestPresentation {
    let (state, status_text) = match status {
        ReadBinaryFileStatus::Completed => {
            let size = size.map_or_else(
                || "unknown size".to_string(),
                |size| format!("{size} bytes"),
            );
            let mime = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());
            let kind = match content_kind {
                Some(ReadBinaryFileContentKind::Image) => "image",
                Some(ReadBinaryFileContentKind::EmbeddedResource) => "binary resource",
                None => "binary content",
            };
            (
                RequestState::Completed,
                format!("Completed · {size} · {mime} · {kind}"),
            )
        }
        ReadBinaryFileStatus::NotFound => (RequestState::Failed, "File not found".to_string()),
        ReadBinaryFileStatus::AccessDenied => (RequestState::Failed, "Access denied".to_string()),
        ReadBinaryFileStatus::NotAFile => (RequestState::Failed, "Not a regular file".to_string()),
        ReadBinaryFileStatus::TooLarge => (
            RequestState::Failed,
            "File exceeds binary read limit".to_string(),
        ),
        ReadBinaryFileStatus::ReadFailed => (RequestState::Failed, "Read failed".to_string()),
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

fn insert_file_presentation(
    status: crate::mcp::InsertFileStatus,
    error: Option<String>,
    inserted_bytes: u64,
) -> RequestPresentation {
    use crate::mcp::InsertFileStatus;

    let (state, status_text) = match status {
        InsertFileStatus::Completed => (
            RequestState::Completed,
            format!("Completed \u{00b7} inserted {inserted_bytes} bytes"),
        ),
        InsertFileStatus::NotFound => (RequestState::Failed, "File not found".to_string()),
        InsertFileStatus::AccessDenied => (RequestState::Failed, "Access denied".to_string()),
        InsertFileStatus::NotAFile => (RequestState::Failed, "Not a regular file".to_string()),
        InsertFileStatus::RangeOutOfBounds => (
            RequestState::Failed,
            "Anchor line out of bounds".to_string(),
        ),
        InsertFileStatus::ReadFailed => (RequestState::Failed, "Read failed".to_string()),
        InsertFileStatus::WriteFailed => (RequestState::Failed, "Write failed".to_string()),
        InsertFileStatus::ReplaceFailed => (RequestState::Failed, "Replace failed".to_string()),
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
        RequestUpdate::GetInstructionsCompleted => RequestPresentation {
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
            ..
        } => launch_process_presentation(status, error, pid, exit_code),
        RequestUpdate::LaunchProcessOutputProgress { pid, .. } => RequestPresentation {
            state: RequestState::InProgress,
            status_text: "In progress".to_string(),
            detail_text: None,
            pid: Some(pid),
        },
        RequestUpdate::ReadFileResponded {
            status,
            error,
            actual_start_line,
            actual_end_line,
            next_start_line,
            eof,
            ..
        } => read_file_presentation(
            status,
            error,
            actual_start_line,
            actual_end_line,
            next_start_line,
            eof,
        ),
        RequestUpdate::ReadBinaryFileResponded {
            status,
            error,
            size,
            mime_type,
            content_kind,
        } => read_binary_file_presentation(status, error, size, mime_type, content_kind),
        RequestUpdate::WriteFileResponded {
            status,
            error,
            replaced_line_count,
            inserted_bytes,
        } => write_file_presentation(status, error, replaced_line_count, inserted_bytes),
        RequestUpdate::InsertFileResponded {
            status,
            error,
            inserted_bytes,
        } => insert_file_presentation(status, error, inserted_bytes),
        RequestUpdate::RequestTimedOut {
            timeout_seconds,
            error,
        } => RequestPresentation {
            state: RequestState::Failed,
            status_text: format!("Request timed out · {timeout_seconds}s limit"),
            detail_text: Some(error),
            pid: None,
        },
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
    let mut excess = requests
        .iter()
        .filter(|request| request.state != RequestState::InProgress)
        .count()
        .saturating_sub(MAX_RECENT_REQUESTS);
    if excess == 0 {
        return;
    }

    requests.retain(|request| {
        let remove = excess != 0 && request.state != RequestState::InProgress;
        excess -= usize::from(remove);
        !remove
    });
}

fn should_show_busy_icon(
    requests: &[RequestEntry],
    last_request_activity: Option<Duration>,
    current_elapsed: Duration,
) -> bool {
    requests
        .iter()
        .any(|request| request.state == RequestState::InProgress)
        || last_request_activity.is_some_and(|last_activity| {
            current_elapsed.saturating_sub(last_activity) < BUSY_ICON_COOLDOWN
        })
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
            stdout_file: None,
            stderr_file: None,
            stdout_lines: 0,
            stderr_lines: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            background_failure: false,
        }),
        UiEventKind::RequestUpdated { id, update } => {
            if let Some(request) = requests.iter_mut().rev().find(|request| request.id == id) {
                if let RequestUpdate::LaunchProcessOutputProgress {
                    pid,
                    stdout_lines,
                    stderr_lines,
                    stdout_truncated,
                    stderr_truncated,
                } = &update
                {
                    request.pid = Some(*pid);
                    request.stdout_lines = *stdout_lines;
                    request.stderr_lines = *stderr_lines;
                    request.stdout_truncated = *stdout_truncated;
                    request.stderr_truncated = *stderr_truncated;
                    return;
                }
                if let RequestUpdate::LaunchProcessResponded {
                    stdout_file,
                    stderr_file,
                    ..
                } = &update
                {
                    request.stdout_file.clone_from(stdout_file);
                    request.stderr_file.clone_from(stderr_file);
                }
                let is_primary_terminal = matches!(
                    &update,
                    RequestUpdate::PingCompleted
                        | RequestUpdate::GetInstructionsCompleted
                        | RequestUpdate::LaunchProcessResponded { .. }
                        | RequestUpdate::ReadFileResponded { .. }
                        | RequestUpdate::ReadBinaryFileResponded { .. }
                        | RequestUpdate::WriteFileResponded { .. }
                        | RequestUpdate::InsertFileResponded { .. }
                        | RequestUpdate::RequestTimedOut { .. }
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

fn launch_output_summary(request: &RequestEntry) -> Option<String> {
    if !matches!(request.request, RequestData::LaunchProcess { .. }) {
        return None;
    }

    let total = request.stdout_lines.saturating_add(request.stderr_lines);
    let unit = if total == 1 { "line" } else { "lines" };
    let mut summary = format!(
        "Output {total} {unit} (stdout {} · stderr {})",
        request.stdout_lines, request.stderr_lines
    );
    match (request.stdout_truncated, request.stderr_truncated) {
        (true, true) => summary.push_str(" · stdout + stderr truncated"),
        (true, false) => summary.push_str(" · stdout truncated"),
        (false, true) => summary.push_str(" · stderr truncated"),
        (false, false) => {}
    }
    Some(summary)
}

fn request_tool_name(request: &RequestData) -> &'static str {
    match request {
        RequestData::Ping => "ping",
        RequestData::GetInstructions => "get_instructions",
        RequestData::LaunchProcess { .. } => "launch_process",
        RequestData::ReadFile { .. } => "read_file",
        RequestData::ReadBinaryFile { .. } => "read_binary_file",
        RequestData::WriteFile { .. } => "write_file",
        RequestData::InsertFile { position, .. } => match position {
            crate::mcp::InsertFilePosition::Before => "insert_before_line",
            crate::mcp::InsertFilePosition::After => "insert_after_line",
        },
    }
}

fn request_summary(request: &RequestEntry) -> String {
    match &request.request {
        RequestData::Ping => "Server health check".to_string(),
        RequestData::GetInstructions => "Get server instructions".to_string(),
        RequestData::LaunchProcess {
            command_line,
            detached,
            ..
        } => {
            let command_line = truncate_with_ellipsis(command_line, MAX_COMMAND_LINE_CHARACTERS);
            let mut summary = request.pid.map_or(command_line.clone(), |pid| {
                format!("{command_line} · PID {pid}")
            });
            if *detached {
                summary.push_str(" · detached requested");
            }
            summary
        }
        RequestData::ReadFile {
            path,
            start_line,
            end_line,
        } => format!("{path} · requested lines {start_line}–{end_line}"),
        RequestData::ReadBinaryFile { path, max_bytes } => max_bytes.map_or_else(
            || format!("{path} · server limit 100 MB"),
            |max_bytes| format!("{path} · requested maximum {max_bytes} bytes"),
        ),
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
        RequestData::InsertFile {
            path,
            line,
            position,
            insertion_bytes,
        } => {
            let relation = match position {
                crate::mcp::InsertFilePosition::Before => "before",
                crate::mcp::InsertFilePosition::After => "after",
            };
            format!(
                "{path} \u{00b7} insert {relation} line {line} \u{00b7} {insertion_bytes} bytes"
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

fn request_summary_tooltip(request: &RequestEntry) -> Option<String> {
    let RequestData::LaunchProcess {
        command_line,
        working_directory,
        detached,
        timeout_ms,
        timeout_action,
    } = &request.request
    else {
        return None;
    };

    let mut lines = vec![
        format!("Request {}", request.id.get()),
        format!("Command: {command_line}"),
        format!(
            "Working directory: {}",
            working_directory
                .as_deref()
                .unwrap_or("<default temporary directory>")
        ),
        format!(
            "Launch mode: {}",
            if *detached { "detached" } else { "foreground" }
        ),
        format!(
            "Timeout: {}",
            match timeout_ms {
                Some(ms) => format!(
                    "{ms} ms ({})",
                    match timeout_action {
                        Some(crate::mcp::TimeoutAction::Detach) => "detach",
                        Some(crate::mcp::TimeoutAction::Stop) => "stop",
                        None => "no action specified",
                    }
                ),
                None => "none".to_string(),
            }
        ),
    ];
    if let Some(stdout_file) = &request.stdout_file {
        lines.push(format!("stdout file: {stdout_file}"));
    }
    if let Some(stderr_file) = &request.stderr_file {
        lines.push(format!("stderr file: {stderr_file}"));
    }
    Some(lines.join("\n"))
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

fn paint_status_dot(ui: &mut egui::Ui, colour: egui::Color32) {
    let (response, painter) = ui.allocate_painter(egui::vec2(12.0, 16.0), egui::Sense::hover());
    painter.circle_filled(response.rect.center(), 4.0, colour);
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
                if let Some(tooltip) = request_summary_tooltip(request) {
                    summary.on_hover_text(tooltip);
                }
                let mut timing = format!(
                    "Started {} · Duration {}",
                    format_start_time(&request.started_at),
                    format_duration(request.duration(current_elapsed))
                );
                if let Some(output) = launch_output_summary(request) {
                    timing.push_str(" · ");
                    timing.push_str(&output);
                }
                ui.weak(timing);
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
    icons: AppIcons,
    busy_icon_active: bool,
    last_request_activity: Option<Duration>,
    status_text: String,
    mcp_endpoint: Option<String>,
    tunnel_launch: Option<TunnelLaunch>,
    tunnel_state: TunnelUiState,
    tunnel_start_automatically: bool,
    automatic_tunnel_launch_pending: bool,
    tunnel_setting_error: Option<String>,
    maximum_request_timeout_seconds: u64,
    maximum_request_timeout_shared: Arc<AtomicU64>,
    maximum_request_timeout_setting_error: Option<String>,
    active_http_connections: usize,
    active_mcp_sessions: usize,
    fatal_error: Option<String>,
    local_instructions_diagnostic: Option<LocalInstructionsDiagnostic>,
    start_time: Instant,
    disk_log: DiskLog,
}

impl RemoteControlApp {
    pub fn new(
        rx: Receiver<UiEvent>,
        start_time: Instant,
        maximum_request_timeout_shared: Arc<AtomicU64>,
        maximum_request_timeout_setting_error: Option<String>,
    ) -> Self {
        let (tunnel_start_automatically, tunnel_setting_error) =
            match tunnel::load_start_automatically() {
                Ok(enabled) => (enabled, None),
                Err(error) => {
                    eprintln!("{error}");
                    (false, Some(error))
                }
            };

        Self::with_tunnel_setting(
            rx,
            start_time,
            tunnel_start_automatically,
            tunnel_setting_error,
            maximum_request_timeout_shared,
            maximum_request_timeout_setting_error,
        )
    }

    fn with_tunnel_setting(
        rx: Receiver<UiEvent>,
        start_time: Instant,
        tunnel_start_automatically: bool,
        tunnel_setting_error: Option<String>,
        maximum_request_timeout_shared: Arc<AtomicU64>,
        maximum_request_timeout_setting_error: Option<String>,
    ) -> Self {
        let maximum_request_timeout_seconds =
            maximum_request_timeout_shared.load(Ordering::Relaxed);
        Self {
            rx,
            requests: Vec::new(),
            icons: AppIcons::load(),
            busy_icon_active: false,
            last_request_activity: None,
            status_text: "Starting".to_string(),
            mcp_endpoint: None,
            tunnel_launch: None,
            tunnel_state: TunnelUiState::Idle,
            tunnel_start_automatically,
            automatic_tunnel_launch_pending: tunnel_start_automatically,
            tunnel_setting_error,
            maximum_request_timeout_seconds,
            maximum_request_timeout_shared,
            maximum_request_timeout_setting_error,
            active_http_connections: 0,
            active_mcp_sessions: 0,
            fatal_error: None,
            local_instructions_diagnostic: None,
            start_time,
            disk_log: DiskLog::open(),
        }
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.disk_log.log_ui_event(&event);
            if matches!(
                &event.kind,
                UiEventKind::RequestStarted { .. } | UiEventKind::RequestUpdated { .. }
            ) {
                self.last_request_activity = Some(event.elapsed);
            }

            match &event.kind {
                UiEventKind::WorkerStarted => self.status_text = "Worker started".to_string(),
                UiEventKind::ServerStarting => {
                    self.status_text = "Server starting".to_string();
                    self.active_http_connections = 0;
                    self.active_mcp_sessions = 0;
                }
                UiEventKind::ServerListening { endpoint } => {
                    self.status_text = "MCP server running".to_string();
                    self.mcp_endpoint = Some(endpoint.clone());
                    if self.automatic_tunnel_launch_pending && self.fatal_error.is_none() {
                        self.automatic_tunnel_launch_pending = false;
                        self.start_tunnel();
                    }
                }
                UiEventKind::HttpConnectionOpened => {
                    self.active_http_connections = self.active_http_connections.saturating_add(1);
                }
                UiEventKind::HttpConnectionClosed => {
                    self.active_http_connections = self.active_http_connections.saturating_sub(1);
                }
                UiEventKind::ClientConnected => {
                    self.active_mcp_sessions = self.active_mcp_sessions.saturating_add(1);
                }
                UiEventKind::ClientDisconnected => {
                    self.active_mcp_sessions = self.active_mcp_sessions.saturating_sub(1);
                }
                UiEventKind::ClientInitialized => {}
                UiEventKind::LocalInstructionsDiagnostic { diagnostic } => {
                    self.local_instructions_diagnostic = Some(diagnostic.clone());
                }
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

    fn update_window_icon(
        &mut self,
        context: &egui::Context,
        _frame: &eframe::Frame,
        current_elapsed: Duration,
    ) {
        let should_be_busy =
            should_show_busy_icon(&self.requests, self.last_request_activity, current_elapsed);
        if should_be_busy == self.busy_icon_active {
            return;
        }

        self.busy_icon_active = should_be_busy;
        let icon = if should_be_busy {
            Arc::clone(&self.icons.busy)
        } else {
            Arc::clone(&self.icons.normal)
        };
        context.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::clone(&icon))));

        #[cfg(target_os = "macos")]
        set_macos_application_icon(&icon);

        #[cfg(target_os = "windows")]
        if let Some(window) = _frame.winit_window() {
            use winit::platform::windows::WindowExtWindows as _;

            let taskbar_icon = if should_be_busy {
                &self.icons.busy
            } else {
                &self.icons.normal
            };
            let taskbar_icon = winit::window::Icon::from_rgba(
                taskbar_icon.rgba.clone(),
                taskbar_icon.width,
                taskbar_icon.height,
            )
            .expect("embedded application icon should have valid RGBA dimensions");
            window.set_taskbar_icon(Some(taskbar_icon));
        }
    }

    fn start_tunnel(&mut self) {
        self.disk_log.log_tunnel(
            "tunnel_launch_requested",
            format!(
                "endpoint={}",
                self.mcp_endpoint.as_deref().unwrap_or("<unavailable>")
            ),
        );
        let endpoint = self.mcp_endpoint.clone().unwrap_or_default();
        match tunnel::start_tunnel(&endpoint) {
            Ok(launch) => {
                self.disk_log.log_tunnel(
                    "tunnel_process_launched",
                    format!("log_path={}", launch.log_path().display()),
                );
                let log_path = launch.log_path().display().to_string();
                self.tunnel_launch = Some(launch);
                self.tunnel_state = TunnelUiState::Starting { log_path };
            }
            Err(error) => {
                self.disk_log.log_tunnel("tunnel_launch_failed", &error);
                self.tunnel_state = TunnelUiState::Failed { error };
            }
        }
    }

    fn stop_tunnel(&mut self) {
        match self.tunnel_state {
            TunnelUiState::Starting { .. } | TunnelUiState::Running { .. } => {}
            TunnelUiState::Idle | TunnelUiState::Failed { .. } => return,
        }

        self.disk_log.log_tunnel("tunnel_stop_requested", "");
        self.tunnel_launch.take();
        self.tunnel_state = TunnelUiState::Idle;
    }

    fn receive_tunnel_event(&mut self) {
        let event = self.tunnel_launch.as_ref().map(TunnelLaunch::try_recv);
        match event {
            Some(Ok(TunnelLaunchEvent::Ready)) => {
                self.disk_log.log_tunnel("tunnel_ready", "");
                let log_path = self
                    .tunnel_launch
                    .as_ref()
                    .map(|launch| launch.log_path().display().to_string())
                    .unwrap_or_default();
                self.tunnel_state = TunnelUiState::Running { log_path };
            }
            Some(Ok(TunnelLaunchEvent::Failed(error))) => {
                self.disk_log.log_tunnel("tunnel_failed", &error);
                self.tunnel_launch.take();
                self.tunnel_state = TunnelUiState::Failed { error };
            }
            Some(Err(TryRecvError::Disconnected)) => {
                if !matches!(self.tunnel_state, TunnelUiState::Failed { .. }) {
                    let error =
                        "The tunnel launcher stopped without reporting a result.".to_string();
                    self.disk_log.log_tunnel("tunnel_disconnected", &error);
                    self.tunnel_launch.take();
                    self.tunnel_state = TunnelUiState::Failed { error };
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn render_connection_panel(&mut self, ui: &mut egui::Ui) {
        let tunnel_starting = matches!(self.tunnel_state, TunnelUiState::Starting { .. });
        let tunnel_running = matches!(self.tunnel_state, TunnelUiState::Running { .. });
        let tunnel_process_active = tunnel_starting || tunnel_running;
        let tunnel_failed = matches!(self.tunnel_state, TunnelUiState::Failed { .. });
        let mut stop_clicked = false;
        let mut start_clicked = false;
        let mut automatic_setting_changed = false;
        let mut maximum_request_timeout_setting_changed = false;
        let previous_automatic_setting = self.tunnel_start_automatically;
        let previous_maximum_request_timeout_seconds = self.maximum_request_timeout_seconds;

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                paint_status_dot(ui, ui.visuals().strong_text_color());
                ui.strong(&self.status_text);
                if let Some(endpoint) = &self.mcp_endpoint {
                    ui.weak(endpoint);
                }

                if let Some(diagnostic) = &self.local_instructions_diagnostic {
                    ui.separator();
                    let path = match diagnostic {
                        LocalInstructionsDiagnostic::Loaded { path }
                        | LocalInstructionsDiagnostic::Warning { path, .. } => path,
                    };
                    let link = ui
                        .link("Local instructions:")
                        .on_hover_text(path.display().to_string());
                    if link.clicked()
                        && let Some(folder) = path.parent()
                        && let Err(error) =
                            std::process::Command::new(if cfg!(target_os = "windows") {
                                "explorer.exe"
                            } else if cfg!(target_os = "macos") {
                                "open"
                            } else {
                                "xdg-open"
                            })
                            .arg(folder)
                            .spawn()
                    {
                        eprintln!(
                            "Failed to open local instructions folder {}: {error}",
                            folder.display()
                        );
                    }
                    ui.strong(
                        if matches!(diagnostic, LocalInstructionsDiagnostic::Loaded { .. }) {
                            "yes"
                        } else {
                            "no"
                        },
                    );
                }
            });

            ui.horizontal(|ui| {
                ui.strong("HTTP connections:");
                ui.label(self.active_http_connections.to_string());

                ui.separator();
                ui.strong("MCP sessions:");
                ui.label(self.active_mcp_sessions.to_string());
            });

            ui.horizontal(|ui| {
                if tunnel_starting {
                    ui.spinner();
                }
                ui.strong("Tunnel client running:");
                let tunnel_status = ui.label(if tunnel_process_active { "yes" } else { "no" });
                match &self.tunnel_state {
                    TunnelUiState::Starting { log_path } | TunnelUiState::Running { log_path } => {
                        tunnel_status.on_hover_text(format!("Tunnel log: {log_path}"));
                    }
                    TunnelUiState::Idle | TunnelUiState::Failed { .. } => {}
                }

                if tunnel_starting {
                    stop_clicked = ui.button("Cancel tunnel launch").clicked();
                } else if tunnel_running {
                    stop_clicked = ui.button("Stop Secure MCP Tunnel").clicked();
                } else {
                    let button_text = if tunnel_failed {
                        "Retry Secure MCP Tunnel"
                    } else {
                        "Start Secure MCP Tunnel"
                    };
                    start_clicked = ui
                        .add_enabled(
                            self.mcp_endpoint.is_some() && self.fatal_error.is_none(),
                            egui::Button::new(button_text),
                        )
                        .clicked();
                }

                automatic_setting_changed = ui
                    .checkbox(&mut self.tunnel_start_automatically, "Start automatically")
                    .changed();

                if let TunnelUiState::Failed { error } = &self.tunnel_state {
                    ui.colored_label(ui.visuals().error_fg_color, "Tunnel launch failed")
                        .on_hover_text(error);
                }
                if let Some(error) = &self.tunnel_setting_error {
                    ui.colored_label(ui.visuals().error_fg_color, "Automatic-start setting error")
                        .on_hover_text(error);
                }
            });

            ui.horizontal(|ui| {
                ui.strong("Maximum request timeout:");
                let response = ui.add(
                    egui::DragValue::new(&mut self.maximum_request_timeout_seconds)
                        .speed(1.0)
                        .suffix(" s"),
                );
                maximum_request_timeout_setting_changed = response.changed();
                response.on_hover_text(
                    "Maximum time allowed for an MCP tool request. Set to 0 to disable the local request timeout. When using ChatGPT's observed ~120 s outer limit, 110 s leaves time to return a useful local error.",
                );
                ui.weak("0 = disabled");
                if let Some(error) = &self.maximum_request_timeout_setting_error {
                    ui.colored_label(ui.visuals().error_fg_color, "Request-timeout setting error")
                        .on_hover_text(error);
                }
            });
        });

        if automatic_setting_changed {
            match tunnel::save_start_automatically(self.tunnel_start_automatically) {
                Ok(()) => {
                    self.disk_log.log_tunnel(
                        "tunnel_auto_start_setting_changed",
                        format!("enabled={}", self.tunnel_start_automatically),
                    );
                    if self.mcp_endpoint.is_none() {
                        self.automatic_tunnel_launch_pending = self.tunnel_start_automatically;
                    }
                    self.tunnel_setting_error = None;
                }
                Err(error) => {
                    eprintln!("{error}");
                    self.disk_log
                        .log_tunnel("tunnel_auto_start_setting_save_failed", &error);
                    self.tunnel_start_automatically = previous_automatic_setting;
                    self.tunnel_setting_error = Some(error);
                }
            }
        }

        if maximum_request_timeout_setting_changed {
            match settings::save_maximum_request_timeout_seconds(
                self.maximum_request_timeout_seconds,
            ) {
                Ok(()) => {
                    self.maximum_request_timeout_shared
                        .store(self.maximum_request_timeout_seconds, Ordering::Relaxed);
                    self.disk_log.log_tunnel(
                        "maximum_request_timeout_setting_changed",
                        format!("seconds={}", self.maximum_request_timeout_seconds),
                    );
                    self.maximum_request_timeout_setting_error = None;
                }
                Err(error) => {
                    eprintln!("{error}");
                    self.disk_log
                        .log_tunnel("maximum_request_timeout_setting_save_failed", &error);
                    self.maximum_request_timeout_seconds = previous_maximum_request_timeout_seconds;
                    self.maximum_request_timeout_setting_error = Some(error);
                }
            }
        }

        if stop_clicked {
            self.stop_tunnel();
        } else if start_clicked {
            self.start_tunnel();
        }
    }

    fn render_hosted(&mut self, ui: &mut egui::Ui, current_elapsed: Duration) {
        self.render_connection_panel(ui);

        if let Some(error) = &self.fatal_error {
            ui.add_space(5.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.colored_label(ui.visuals().error_fg_color, "Fatal server error");
                ui.label(error);
            });
        }

        ui.add_space(6.0);
        if self.requests.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak("MCP requests will show here");
            });
        } else {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for request in self.requests.iter().rev() {
                        render_request_row(ui, request, current_elapsed);
                        ui.add_space(4.0);
                    }
                });
        }
    }
}

impl eframe::App for RemoteControlApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.receive_events();
        self.receive_tunnel_event();
        let current_elapsed = self.start_time.elapsed();
        self.update_window_icon(ui.ctx(), frame, current_elapsed);

        egui::CentralPanel::default().show(ui, |ui| {
            self.render_hosted(ui, current_elapsed);
        });

        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn test_app(rx: Receiver<UiEvent>) -> RemoteControlApp {
        RemoteControlApp::with_tunnel_setting(
            rx,
            Instant::now(),
            false,
            None,
            Arc::new(AtomicU64::new(0)),
            None,
        )
    }

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
    fn app_starts_while_the_http_worker_prepares_the_listener() {
        let (_tx, rx) = mpsc::channel();
        let app = test_app(rx);

        assert_eq!(app.status_text, "Starting");
        assert!(app.mcp_endpoint.is_none());
        assert!(app.requests.is_empty());
        assert!(app.tunnel_launch.is_none());
        assert!(matches!(app.tunnel_state, TunnelUiState::Idle));
        assert_eq!(app.active_http_connections, 0);
        assert_eq!(app.active_mcp_sessions, 0);
    }

    #[test]
    fn cancelling_tunnel_launch_returns_to_idle_state() {
        let (_tx, rx) = mpsc::channel();
        let mut app = test_app(rx);
        app.status_text = "MCP server running".to_string();
        app.tunnel_state = TunnelUiState::Starting {
            log_path: "tunnel.log".to_string(),
        };

        app.stop_tunnel();

        assert!(matches!(app.tunnel_state, TunnelUiState::Idle));
        assert!(app.tunnel_launch.is_none());
        assert_eq!(app.status_text, "MCP server running");
    }

    #[test]
    fn stopping_running_tunnel_returns_to_idle_state() {
        let (_tx, rx) = mpsc::channel();
        let mut app = test_app(rx);
        app.status_text = "MCP server running".to_string();
        app.active_http_connections = 3;
        app.active_mcp_sessions = 2;
        app.tunnel_state = TunnelUiState::Running {
            log_path: "tunnel.log".to_string(),
        };

        app.stop_tunnel();

        assert!(matches!(app.tunnel_state, TunnelUiState::Idle));
        assert!(app.tunnel_launch.is_none());
        assert_eq!(app.active_http_connections, 3);
        assert_eq!(app.active_mcp_sessions, 2);
        assert_eq!(app.status_text, "MCP server running");
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
    fn busy_icon_stays_active_while_a_request_is_running_and_during_cooldown() {
        let mut requests = Vec::new();
        assert!(!should_show_busy_icon(
            &requests,
            None,
            Duration::from_secs(20)
        ));

        apply_request_event(&mut requests, started_event(1, Duration::from_secs(1)));
        assert!(should_show_busy_icon(
            &requests,
            Some(Duration::from_secs(1)),
            Duration::from_secs(20)
        ));

        apply_request_event(
            &mut requests,
            updated_event(1, Duration::from_secs(20), RequestUpdate::PingCompleted),
        );
        assert!(should_show_busy_icon(
            &requests,
            Some(Duration::from_secs(20)),
            Duration::from_millis(49_999)
        ));
        assert!(!should_show_busy_icon(
            &requests,
            Some(Duration::from_secs(20)),
            Duration::from_secs(50)
        ));
    }

    #[test]
    fn busy_icon_remains_active_until_overlapping_requests_finish() {
        let mut requests = Vec::new();
        apply_request_event(&mut requests, started_event(1, Duration::from_secs(1)));
        apply_request_event(&mut requests, started_event(2, Duration::from_secs(2)));
        apply_request_event(
            &mut requests,
            updated_event(1, Duration::from_secs(3), RequestUpdate::PingCompleted),
        );

        assert!(should_show_busy_icon(
            &requests,
            Some(Duration::from_secs(3)),
            Duration::from_secs(30)
        ));

        apply_request_event(
            &mut requests,
            updated_event(2, Duration::from_secs(30), RequestUpdate::PingCompleted),
        );
        assert!(!should_show_busy_icon(
            &requests,
            Some(Duration::from_secs(30)),
            Duration::from_secs(60)
        ));
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
    fn launch_output_progress_updates_live_and_after_detached_terminal_response() {
        let mut requests = Vec::new();
        apply_request_event(
            &mut requests,
            UiEvent {
                elapsed: Duration::from_secs(2),
                kind: UiEventKind::RequestStarted {
                    id: RequestId(7),
                    request: RequestData::LaunchProcess {
                        command_line: "worker.exe".to_string(),
                        working_directory: None,
                        detached: true,
                        timeout_ms: None,
                        timeout_action: None,
                    },
                    started_at: Local::now(),
                },
            },
        );
        apply_request_event(
            &mut requests,
            updated_event(
                7,
                Duration::from_secs(3),
                RequestUpdate::LaunchProcessOutputProgress {
                    pid: 42,
                    stdout_lines: 5,
                    stderr_lines: 2,
                    stdout_truncated: true,
                    stderr_truncated: false,
                },
            ),
        );
        assert_eq!(requests[0].state, RequestState::InProgress);
        assert!(requests[0].finished_duration.is_none());
        assert_eq!(requests[0].pid, Some(42));
        assert_eq!(
            launch_output_summary(&requests[0]).as_deref(),
            Some("Output 7 lines (stdout 5 · stderr 2) · stdout truncated")
        );

        apply_request_event(
            &mut requests,
            updated_event(
                7,
                Duration::from_secs(4),
                RequestUpdate::LaunchProcessResponded {
                    status: LaunchProcessStatus::Detached,
                    error: None,
                    pid: Some(42),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    stdout_file: Some("stdout.log".to_string()),
                    stderr_file: Some("stderr.log".to_string()),
                },
            ),
        );
        assert_eq!(requests[0].state, RequestState::Warning);
        assert_eq!(requests[0].finished_duration, Some(Duration::from_secs(2)));

        apply_request_event(
            &mut requests,
            updated_event(
                7,
                Duration::from_secs(6),
                RequestUpdate::LaunchProcessOutputProgress {
                    pid: 42,
                    stdout_lines: 8,
                    stderr_lines: 3,
                    stdout_truncated: true,
                    stderr_truncated: true,
                },
            ),
        );
        assert_eq!(requests[0].state, RequestState::Warning);
        assert_eq!(requests[0].finished_duration, Some(Duration::from_secs(2)));
        assert_eq!(
            launch_output_summary(&requests[0]).as_deref(),
            Some("Output 11 lines (stdout 8 · stderr 3) · stdout + stderr truncated")
        );
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
                        working_directory: Some(r"C:\work".to_string()),
                        detached: true,
                        timeout_ms: None,
                        timeout_action: None,
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
                    stdout: None,
                    stderr: None,
                    stdout_file: Some("stdout.log".to_string()),
                    stderr_file: Some("stderr.log".to_string()),
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
        assert_eq!(requests[0].stdout_file.as_deref(), Some("stdout.log"));
        assert_eq!(requests[0].stderr_file.as_deref(), Some("stderr.log"));
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
    fn active_requests_are_retained_beyond_the_recent_history_limit() {
        let mut requests = Vec::new();
        for id in 1..=(MAX_RECENT_REQUESTS as u64 + 2) {
            apply_request_event(&mut requests, started_event(id, Duration::from_secs(id)));
        }
        assert_eq!(requests.len(), MAX_RECENT_REQUESTS + 2);
        assert!(
            requests
                .iter()
                .all(|request| request.state == RequestState::InProgress)
        );

        for id in 1..=(MAX_RECENT_REQUESTS as u64 + 1) {
            apply_request_event(
                &mut requests,
                updated_event(
                    id,
                    Duration::from_secs(700 + id),
                    RequestUpdate::PingCompleted,
                ),
            );
        }

        assert_eq!(requests.len(), MAX_RECENT_REQUESTS + 1);
        assert!(!requests.iter().any(|request| request.id == RequestId(1)));
        assert_eq!(
            requests.last().unwrap().id,
            RequestId(MAX_RECENT_REQUESTS as u64 + 2)
        );
        assert_eq!(requests.last().unwrap().state, RequestState::InProgress);
    }

    #[test]
    fn finished_requests_are_capped_and_oldest_is_removed_first() {
        let mut requests = Vec::new();
        for id in 1..=(MAX_RECENT_REQUESTS as u64 + 1) {
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
        assert_eq!(requests.len(), MAX_RECENT_REQUESTS);
        assert_eq!(requests.first().unwrap().id, RequestId(2));
        assert_eq!(
            requests.last().unwrap().id,
            RequestId(MAX_RECENT_REQUESTS as u64 + 1)
        );
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
                working_directory: Some(r"C:\work".to_string()),
                detached: false,
                timeout_ms: Some(5000),
                timeout_action: Some(crate::mcp::TimeoutAction::Stop),
            },
            started_at: Local::now(),
            started_elapsed: Duration::ZERO,
            finished_duration: None,
            state: RequestState::InProgress,
            status_text: "In progress".to_string(),
            detail_text: None,
            pid: Some(42),
            stdout_file: None,
            stderr_file: None,
            stdout_lines: 0,
            stderr_lines: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            background_failure: false,
        };
        assert_eq!(
            request_summary(&launch),
            "safe.exe visible argument · PID 42"
        );
        assert_eq!(
            request_summary_tooltip(&launch).as_deref(),
            Some(
                "Request 1\nCommand: safe.exe visible argument\nWorking directory: C:\\work\nLaunch mode: foreground\nTimeout: 5000 ms (stop)"
            )
        );

        let detached_launch = RequestEntry {
            request: RequestData::LaunchProcess {
                command_line: "worker.exe".to_string(),
                working_directory: None,
                detached: true,
                timeout_ms: None,
                timeout_action: None,
            },
            pid: Some(84),
            ..launch
        };
        assert_eq!(
            request_summary(&detached_launch),
            "worker.exe · PID 84 · detached requested"
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
        let mut app = test_app(rx);
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ServerListening {
                endpoint: "http://127.0.0.1:61337/mcp".to_string(),
            },
        })
        .unwrap();
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::HttpConnectionOpened,
        })
        .unwrap();
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ClientConnected,
        })
        .unwrap();
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ClientInitialized,
        })
        .unwrap();
        let diagnostic = LocalInstructionsDiagnostic::Warning {
            path: std::path::PathBuf::from("C:\\missing\\instructions\\LOCAL.md"),
            message: "file not found".to_string(),
        };
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::LocalInstructionsDiagnostic {
                diagnostic: diagnostic.clone(),
            },
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
        assert_eq!(app.active_http_connections, 1);
        assert_eq!(app.active_mcp_sessions, 1);
        assert_eq!(
            app.mcp_endpoint.as_deref(),
            Some("http://127.0.0.1:61337/mcp")
        );
        assert_eq!(app.fatal_error.as_deref(), Some("fatal detail"));
        assert_eq!(app.local_instructions_diagnostic, Some(diagnostic));
    }

    #[test]
    fn http_connection_and_mcp_session_counts_update_independently() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = test_app(rx);
        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ServerListening {
                endpoint: "http://127.0.0.1:61337/mcp".to_string(),
            },
        })
        .unwrap();
        for kind in [
            UiEventKind::HttpConnectionOpened,
            UiEventKind::HttpConnectionOpened,
            UiEventKind::HttpConnectionOpened,
            UiEventKind::ClientConnected,
            UiEventKind::ClientInitialized,
            UiEventKind::ClientConnected,
            UiEventKind::ClientInitialized,
        ] {
            tx.send(UiEvent {
                elapsed: Duration::ZERO,
                kind,
            })
            .unwrap();
        }

        app.receive_events();

        assert_eq!(app.active_http_connections, 3);
        assert_eq!(app.active_mcp_sessions, 2);
        assert_eq!(app.status_text, "MCP server running");

        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::ClientDisconnected,
        })
        .unwrap();
        app.receive_events();

        assert_eq!(app.active_http_connections, 3);
        assert_eq!(app.active_mcp_sessions, 1);
        assert_eq!(app.status_text, "MCP server running");

        tx.send(UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::HttpConnectionClosed,
        })
        .unwrap();
        app.receive_events();

        assert_eq!(app.active_http_connections, 2);
        assert_eq!(app.active_mcp_sessions, 1);
        assert_eq!(app.status_text, "MCP server running");
    }
}
