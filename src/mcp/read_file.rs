use crate::mcp::{McpServer, ReadFileRequest, ReadFileResult, ReadFileStatus, UiEventKind};
use std::io::BufRead;
use std::path::{Component, Path, PathBuf};

const MAX_REQUESTED_LINES: u64 = 500;
const MAX_FILE_BYTES: usize = 256 * 1024;

#[cfg(test)]
pub(crate) use test_hooks::install as install_blocking_test_hook;

pub(crate) fn validate_read_file_request(req: &ReadFileRequest) -> Result<PathBuf, String> {
    if req.path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if req.path.contains('\0') {
        return Err("path cannot contain null characters".to_string());
    }
    if req.start_line == 0 {
        return Err("start_line must be at least 1".to_string());
    }
    if req.end_line == 0 {
        return Err("end_line must be at least 1".to_string());
    }
    if req.start_line > req.end_line {
        return Err("start_line must be less than or equal to end_line".to_string());
    }
    if req.end_line - req.start_line >= MAX_REQUESTED_LINES {
        return Err(format!(
            "requested line range cannot exceed {MAX_REQUESTED_LINES} lines"
        ));
    }

    let requested_path = Path::new(&req.path);
    if !requested_path.is_absolute()
        && matches!(
            requested_path.components().next(),
            Some(Component::Prefix(_) | Component::RootDir)
        )
    {
        return Err("path must be fully qualified or an ordinary relative path".to_string());
    }

    #[cfg(target_os = "windows")]
    if req.path.starts_with('\\') && !req.path.starts_with("\\\\") {
        return Err("root-relative Windows paths are not supported".to_string());
    }

    let resolved = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        std::env::temp_dir().join(requested_path)
    };
    std::path::absolute(resolved)
        .map_err(|error| format!("path could not be resolved to an absolute path: {error}"))
}

fn failure_result(
    req: &ReadFileRequest,
    path: &Path,
    status: ReadFileStatus,
    error: impl Into<String>,
) -> ReadFileResult {
    ReadFileResult {
        status,
        error: Some(error.into()),
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        actual_start_line: None,
        actual_end_line: None,
        text: String::new(),
        eof: None,
        next_start_line: None,
        lossy_utf8: false,
    }
}

fn io_failure(req: &ReadFileRequest, path: &Path, error: std::io::Error) -> ReadFileResult {
    let status = match error.kind() {
        std::io::ErrorKind::NotFound => ReadFileStatus::NotFound,
        std::io::ErrorKind::PermissionDenied => ReadFileStatus::AccessDenied,
        _ => ReadFileStatus::ReadFailed,
    };
    failure_result(req, path, status, error.to_string())
}

fn completed_empty_result(req: &ReadFileRequest, path: &Path) -> ReadFileResult {
    ReadFileResult {
        status: ReadFileStatus::Completed,
        error: None,
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        actual_start_line: None,
        actual_end_line: None,
        text: String::new(),
        eof: Some(true),
        next_start_line: None,
        lossy_utf8: false,
    }
}

fn skip_line(reader: &mut impl BufRead) -> std::io::Result<bool> {
    let mut saw_bytes = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(saw_bytes);
        }
        saw_bytes = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

enum BoundedLine {
    Eof,
    Complete(Vec<u8>),
    TooLong,
}

fn read_line_bounded(
    reader: &mut impl BufRead,
    maximum_content_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(BoundedLine::Eof)
            } else if line.len() > maximum_content_bytes {
                Ok(BoundedLine::TooLong)
            } else {
                Ok(BoundedLine::Complete(line))
            };
        }

        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            if line.len().saturating_add(consumed) > maximum_content_bytes.saturating_add(2) {
                return Ok(BoundedLine::TooLong);
            }
            line.extend_from_slice(&buffer[..consumed]);
            reader.consume(consumed);
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return if line.len() > maximum_content_bytes {
                Ok(BoundedLine::TooLong)
            } else {
                Ok(BoundedLine::Complete(line))
            };
        }

        if line.len().saturating_add(buffer.len()) > maximum_content_bytes.saturating_add(1) {
            return Ok(BoundedLine::TooLong);
        }
        let consumed = buffer.len();
        line.extend_from_slice(buffer);
        reader.consume(consumed);
    }
}

pub(crate) fn open_regular_file_with_metadata(
    req: &ReadFileRequest,
    path: &Path,
    opened_metadata: impl FnOnce(&std::fs::File) -> std::io::Result<std::fs::Metadata>,
) -> Result<std::fs::File, Box<ReadFileResult>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Err(Box::new(io_failure(req, path, error))),
    };
    if !metadata.is_file() {
        return Err(Box::new(failure_result(
            req,
            path,
            ReadFileStatus::NotAFile,
            "The resolved path is not a regular file",
        )));
    }

    // The pathname check gives useful early classification, while metadata from
    // the opened handle prevents reading a different non-file swapped in before open.
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return Err(Box::new(io_failure(req, path, error))),
    };
    let metadata = match opened_metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) => return Err(Box::new(io_failure(req, path, error))),
    };
    if !metadata.is_file() {
        return Err(Box::new(failure_result(
            req,
            path,
            ReadFileStatus::NotAFile,
            "The opened path is not a regular file",
        )));
    }

    Ok(file)
}

fn read_file_blocking(req: ReadFileRequest, path: PathBuf) -> ReadFileResult {
    #[cfg(test)]
    test_hooks::wait_if_installed(&path);

    let file = match open_regular_file_with_metadata(&req, &path, std::fs::File::metadata) {
        Ok(file) => file,
        Err(result) => return *result,
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line_number = 1_u64;

    while line_number < req.start_line {
        match skip_line(&mut reader) {
            Ok(false) => return completed_empty_result(&req, &path),
            Ok(true) => line_number += 1,
            Err(error) => return io_failure(&req, &path, error),
        }
    }

    let mut selected = Vec::new();
    let mut actual_start_line = None;
    let mut actual_end_line = None;
    let mut eof = false;

    while line_number <= req.end_line {
        let maximum_line_bytes = MAX_FILE_BYTES + usize::from(line_number == 1) * 3;
        let mut line = match read_line_bounded(&mut reader, maximum_line_bytes) {
            Ok(BoundedLine::Eof) => {
                eof = true;
                break;
            }
            Ok(BoundedLine::Complete(line)) => line,
            Ok(BoundedLine::TooLong) if actual_start_line.is_none() => {
                return failure_result(
                    &req,
                    &path,
                    ReadFileStatus::LineTooLong,
                    format!("Line {line_number} exceeds the {MAX_FILE_BYTES}-byte limit"),
                );
            }
            Ok(BoundedLine::TooLong) => {
                return ReadFileResult {
                    status: ReadFileStatus::Truncated,
                    error: None,
                    path: path.to_string_lossy().into_owned(),
                    requested_start_line: req.start_line,
                    requested_end_line: req.end_line,
                    actual_start_line,
                    actual_end_line,
                    lossy_utf8: std::str::from_utf8(&selected).is_err(),
                    text: String::from_utf8_lossy(&selected).into_owned(),
                    eof: Some(false),
                    next_start_line: Some(line_number),
                };
            }
            Err(error) => return io_failure(&req, &path, error),
        };
        if line_number == 1 && line.starts_with(&[0xEF, 0xBB, 0xBF]) {
            line.drain(..3);
        }

        let has_selected_line = actual_start_line.is_some();
        let separator_bytes = usize::from(has_selected_line);
        let contribution = separator_bytes.saturating_add(line.len());
        if selected.len().saturating_add(contribution) > MAX_FILE_BYTES {
            if !has_selected_line && line.len() > MAX_FILE_BYTES {
                return failure_result(
                    &req,
                    &path,
                    ReadFileStatus::LineTooLong,
                    format!("Line {line_number} exceeds the {MAX_FILE_BYTES}-byte limit"),
                );
            }

            return ReadFileResult {
                status: ReadFileStatus::Truncated,
                error: None,
                path: path.to_string_lossy().into_owned(),
                requested_start_line: req.start_line,
                requested_end_line: req.end_line,
                actual_start_line,
                actual_end_line,
                lossy_utf8: std::str::from_utf8(&selected).is_err(),
                text: String::from_utf8_lossy(&selected).into_owned(),
                eof: Some(false),
                next_start_line: Some(line_number),
            };
        }

        if separator_bytes != 0 {
            selected.push(b'\n');
        }
        selected.extend_from_slice(&line);
        actual_start_line.get_or_insert(line_number);
        actual_end_line = Some(line_number);

        if line_number == req.end_line {
            eof = match reader.fill_buf() {
                Ok(remaining) => remaining.is_empty(),
                Err(error) => return io_failure(&req, &path, error),
            };
            break;
        }
        line_number += 1;
    }

    if actual_start_line.is_none() {
        return completed_empty_result(&req, &path);
    }

    ReadFileResult {
        status: ReadFileStatus::Completed,
        error: None,
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        actual_start_line,
        actual_end_line,
        lossy_utf8: std::str::from_utf8(&selected).is_err(),
        text: String::from_utf8_lossy(&selected).into_owned(),
        eof: Some(eof),
        next_start_line: None,
    }
}

pub(crate) fn read_file_summary(result: &ReadFileResult) -> String {
    match result.status {
        ReadFileStatus::Completed => match (result.actual_start_line, result.actual_end_line) {
            (Some(start), Some(end)) => {
                let mut summary = format!("Read lines {start}-{end} from {}.", result.path);
                if result.eof == Some(true) && end < result.requested_end_line {
                    summary.push_str(" End of file reached.");
                }
                summary
            }
            _ => format!(
                "No lines returned from {}; start line {} is beyond the end of the file.",
                result.path, result.requested_start_line
            ),
        },
        ReadFileStatus::Truncated => format!(
            "Read lines {}-{} from {}. Result truncated at 256 KiB; continue from line {}.",
            result
                .actual_start_line
                .unwrap_or(result.requested_start_line),
            result
                .actual_end_line
                .unwrap_or(result.requested_start_line),
            result.path,
            result
                .next_start_line
                .unwrap_or(result.requested_start_line)
        ),
        ReadFileStatus::NotFound => format!("File not found: {}.", result.path),
        ReadFileStatus::AccessDenied => format!("Access denied reading {}.", result.path),
        ReadFileStatus::NotAFile => {
            format!("Path is not a regular file: {}.", result.path)
        }
        ReadFileStatus::ReadFailed => format!("Reading {} failed.", result.path),
        ReadFileStatus::LineTooLong => format!(
            "Line {} in {} exceeds the 256 KiB limit.",
            result.requested_start_line, result.path
        ),
    }
}

impl McpServer {
    pub async fn read_file_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<ReadFileRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req = params.0;
        let path = match validate_read_file_request(&req) {
            Ok(path) => path,
            Err(error) => {
                self.send_event(UiEventKind::ReadFileRejected {
                    error: error.clone(),
                });
                return Err(rmcp::ErrorData::invalid_params(error, None));
            }
        };

        self.send_event(UiEventKind::ReadFileRequested {
            path: req.path.clone(),
            start_line: req.start_line,
            end_line: req.end_line,
        });

        let fallback_req = req.clone();
        let fallback_path = path.clone();
        let result = match tokio::task::spawn_blocking(move || read_file_blocking(req, path)).await
        {
            Ok(result) => result,
            Err(error) => failure_result(
                &fallback_req,
                &fallback_path,
                ReadFileStatus::ReadFailed,
                format!("Blocking file-read task failed: {error}"),
            ),
        };

        self.send_event(UiEventKind::ReadFileResponded {
            status: result.status,
            actual_start_line: result.actual_start_line,
            actual_end_line: result.actual_end_line,
        });

        let summary = read_file_summary(&result);
        Self::structured_success(summary, &result)
    }
}

#[cfg(test)]
mod test_hooks {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    struct BlockingHook {
        path: PathBuf,
        started: Sender<()>,
        release: Receiver<()>,
    }

    static BLOCKING_HOOK: Mutex<Option<BlockingHook>> = Mutex::new(None);

    pub(crate) fn install(path: PathBuf, started: Sender<()>, release: Receiver<()>) {
        *BLOCKING_HOOK.lock().unwrap() = Some(BlockingHook {
            path,
            started,
            release,
        });
    }

    pub(super) fn wait_if_installed(path: &Path) {
        let hook = {
            let mut hook = BLOCKING_HOOK.lock().unwrap();
            if hook.as_ref().is_some_and(|hook| hook.path == path) {
                hook.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            let _ = hook.started.send(());
            let _ = hook.release.recv();
        }
    }
}
