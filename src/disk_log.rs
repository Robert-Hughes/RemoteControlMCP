use crate::mcp::{RequestData, RequestUpdate, UiEvent, UiEventKind};
use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
#[cfg(test)]
use std::path::Path;

const MAX_FIELD_CHARS: usize = 4096;

pub struct DiskLog {
    writer: Option<BufWriter<File>>,
}

impl DiskLog {
    pub fn open() -> Self {
        let directory = std::env::temp_dir().join("RemoteControlMCP");
        let timestamp = Local::now().format("%Y%m%d-%H%M%S%.3f");
        let path = directory.join(format!(
            "remote-control-mcp-{}-{}.log",
            std::process::id(),
            timestamp
        ));
        let writer = std::fs::create_dir_all(&directory)
            .and_then(|_| OpenOptions::new().create_new(true).write(true).open(&path))
            .ok()
            .map(BufWriter::new);
        let mut log = Self { writer };
        log.write("application_started", None, "Remote Control MCP started");
        log
    }

    #[cfg(test)]
    fn open_at(path: &Path) -> Self {
        let writer = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .ok()
            .map(BufWriter::new);
        Self { writer }
    }

    pub fn log_ui_event(&mut self, event: &UiEvent) {
        match &event.kind {
            UiEventKind::RequestStarted { id, request, .. } => {
                self.write("request_started", Some(id.get()), &format_request(request))
            }
            UiEventKind::RequestUpdated { id, update } => {
                self.write("request_updated", Some(id.get()), &format_update(update))
            }
            kind => self.write("lifecycle", None, &format!("{kind:?}")),
        }
    }

    pub fn log_tunnel(&mut self, event: &str, details: impl AsRef<str>) {
        self.write(event, None, details.as_ref());
    }

    fn write(&mut self, event: &str, request_id: Option<u64>, details: &str) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let timestamp = Local::now().to_rfc3339();
        let request = request_id
            .map(|id| format!(" request_id={id}"))
            .unwrap_or_default();
        let details = truncate(details, MAX_FIELD_CHARS)
            .replace('\r', "\\rr")
            .replace('\n', "\\n");
        let _ = writeln!(
            writer,
            "{timestamp} event={event}{request} details={details}"
        );
        let _ = writer.flush();
    }
}

fn format_request(request: &RequestData) -> String {
    format!("parameters={request:?}")
}

fn format_update(update: &RequestUpdate) -> String {
    format!("result={update:?}")
}

fn truncate(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let kept: String = value.chars().take(max_chars).collect();
    format!("{kept}… [truncated {} characters]", count - max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{RequestId, UiEventKind};
    use std::time::Duration;

    #[test]
    fn logs_request_id_results_and_truncates_long_fields() {
        let path =
            std::env::temp_dir().join(format!("rmcp-disk-log-test-{}.log", std::process::id()));
        let mut log = DiskLog::open_at(&path);
        log.log_ui_event(&UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::RequestUpdated {
                id: RequestId(42),
                update: RequestUpdate::InternalFailure {
                    error: "x".repeat(MAX_FIELD_CHARS + 50),
                },
            },
        });
        log.log_ui_event(&UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::RequestUpdated {
                id: RequestId(43),
                update: RequestUpdate::LaunchProcessResponded {
                    status: crate::mcp::LaunchProcessStatus::Completed,
                    error: None,
                    pid: Some(123),
                    exit_code: Some(0),
                    stdout: Some("inline stdout".to_string()),
                    stderr: Some("inline stderr".to_string()),
                    stdout_file: None,
                    stderr_file: None,
                },
            },
        });
        log.log_ui_event(&UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::RequestUpdated {
                id: RequestId(44),
                update: RequestUpdate::ReadFileResponded {
                    status: crate::mcp::ReadFileStatus::Completed,
                    error: None,
                    actual_start_line: Some(1),
                    actual_end_line: Some(1),
                    next_start_line: None,
                    eof: Some(true),
                    text: "returned file text".to_string(),
                },
            },
        });
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("event=request_updated request_id=42"));
        assert!(text.contains("result=InternalFailure"));
        assert!(text.contains("inline stdout"));
        assert!(text.contains("inline stderr"));
        assert!(text.contains("returned file text"));
        assert!(text.contains("[truncated"));
        assert!(text.len() < MAX_FIELD_CHARS + 1_500);
    }

    #[test]
    fn logs_lifecycle_and_tunnel_events() {
        let path = std::env::temp_dir().join(format!(
            "rmcp-disk-log-lifecycle-test-{}.log",
            std::process::id()
        ));
        let mut log = DiskLog::open_at(&path);
        log.log_ui_event(&UiEvent {
            elapsed: Duration::ZERO,
            kind: UiEventKind::HttpConnectionOpened,
        });
        log.log_tunnel("tunnel_process_launched", "log_path=C:\\temp\\tunnel.log");
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("details=HttpConnectionOpened"));
        assert!(text.contains("event=tunnel_process_launched"));
    }
}
