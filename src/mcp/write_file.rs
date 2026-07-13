use crate::mcp::file_path::validate_line_file_path;
use crate::mcp::{
    McpServer, RequestData, RequestUpdate, WriteFileRequest, WriteFileResult, WriteFileStatus,
};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_REPLACEMENT_BYTES: usize = 256 * 1024;
const STAGE_FILE_ATTEMPTS: usize = 100;

static NEXT_STAGE_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) use test_hooks::install as install_blocking_test_hook;

pub(crate) fn validate_write_file_request(req: &WriteFileRequest) -> Result<PathBuf, String> {
    let path = validate_line_file_path(&req.path, req.start_line, req.end_line)?;
    if req.text.len() > MAX_REPLACEMENT_BYTES {
        return Err(format!(
            "text cannot exceed {MAX_REPLACEMENT_BYTES} UTF-8 bytes"
        ));
    }
    Ok(path)
}

fn failure_result(
    req: &WriteFileRequest,
    path: &Path,
    status: WriteFileStatus,
    error: impl Into<String>,
) -> WriteFileResult {
    WriteFileResult {
        status,
        error: Some(error.into()),
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        replaced_line_count: None,
        inserted_bytes: 0,
    }
}

fn io_status(error: &std::io::Error, fallback: WriteFileStatus) -> WriteFileStatus {
    match error.kind() {
        std::io::ErrorKind::NotFound => WriteFileStatus::NotFound,
        std::io::ErrorKind::PermissionDenied => WriteFileStatus::AccessDenied,
        std::io::ErrorKind::NotADirectory => WriteFileStatus::ParentNotADirectory,
        _ => fallback,
    }
}

fn io_failure(
    req: &WriteFileRequest,
    path: &Path,
    error: std::io::Error,
    fallback: WriteFileStatus,
) -> WriteFileResult {
    let status = io_status(&error, fallback);
    failure_result(req, path, status, error.to_string())
}

struct StagedFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    committed: bool,
}

impl StagedFile {
    fn close(&mut self) -> std::io::Result<()> {
        if let Some(file) = self.file.take() {
            file.sync_all()?;
        }
        Ok(())
    }

    fn mark_committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.file.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_staged_file(target: &Path) -> std::io::Result<StagedFile> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        )
    })?;

    for _ in 0..STAGE_FILE_ATTEMPTS {
        let id = NEXT_STAGE_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let stage_path = parent.join(format!(
            ".remote-control-mcp-write-{}-{id}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage_path)
        {
            Ok(file) => {
                return Ok(StagedFile {
                    path: stage_path,
                    file: Some(file),
                    committed: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique staging file",
    ))
}

#[derive(Debug)]
enum ExactCopyError {
    Read(std::io::Error),
    Write(std::io::Error),
}

fn copy_line_exact(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<bool, ExactCopyError> {
    let mut saw_bytes = false;
    loop {
        let buffer = reader.fill_buf().map_err(ExactCopyError::Read)?;
        if buffer.is_empty() {
            return Ok(saw_bytes);
        }
        saw_bytes = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        writer
            .write_all(&buffer[..consumed])
            .map_err(ExactCopyError::Write)?;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn copy_remaining_exact(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(), ExactCopyError> {
    loop {
        let buffer = reader.fill_buf().map_err(ExactCopyError::Read)?;
        if buffer.is_empty() {
            return Ok(());
        }
        writer.write_all(buffer).map_err(ExactCopyError::Write)?;
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

fn exact_copy_failure(
    req: &WriteFileRequest,
    path: &Path,
    error: ExactCopyError,
) -> WriteFileResult {
    match error {
        ExactCopyError::Read(error) => io_failure(req, path, error, WriteFileStatus::ReadFailed),
        ExactCopyError::Write(error) => io_failure(req, path, error, WriteFileStatus::WriteFailed),
    }
}

#[derive(Clone, Copy)]
struct SkippedLine {
    exists: bool,
    terminator: Option<&'static [u8]>,
}

fn skip_line_with_terminator(
    reader: &mut impl BufRead,
    mut saw_bytes: bool,
) -> std::io::Result<SkippedLine> {
    let mut previous_byte = None;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(SkippedLine {
                exists: saw_bytes,
                terminator: None,
            });
        }
        saw_bytes = true;

        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let byte_before_newline = if newline == 0 {
                previous_byte
            } else {
                Some(buffer[newline - 1])
            };
            reader.consume(newline + 1);
            return Ok(SkippedLine {
                exists: true,
                terminator: Some(if byte_before_newline == Some(b'\r') {
                    b"\r\n"
                } else {
                    b"\n"
                }),
            });
        }

        previous_byte = buffer.last().copied();
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

fn open_existing_regular_file(
    req: &WriteFileRequest,
    display_path: &Path,
    open_path: &Path,
    pathname_metadata: std::fs::Metadata,
) -> Result<(std::fs::File, std::fs::Metadata), WriteFileResult> {
    if !pathname_metadata.is_file() {
        return Err(failure_result(
            req,
            display_path,
            WriteFileStatus::NotAFile,
            "The resolved path is not a regular file",
        ));
    }

    let file = std::fs::File::open(open_path)
        .map_err(|error| io_failure(req, display_path, error, WriteFileStatus::ReadFailed))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| io_failure(req, display_path, error, WriteFileStatus::ReadFailed))?;
    if !opened_metadata.is_file() {
        return Err(failure_result(
            req,
            display_path,
            WriteFileStatus::NotAFile,
            "The opened path is not a regular file",
        ));
    }

    Ok((file, opened_metadata))
}

#[cfg(target_os = "windows")]
fn commit_replacement(stage: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *const std::ffi::c_void,
            reserved: *const std::ffi::c_void,
        ) -> i32;
    }

    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let stage_wide = stage
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ReplaceFileW(
            target_wide.as_ptr(),
            stage_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn commit_replacement(stage: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(stage, target)
}

fn commit_creation(stage: &Path, target: &Path) -> std::io::Result<()> {
    // Publishing by hard link gives create-new semantics on every supported
    // platform: the operation fails if another actor created the target first.
    std::fs::hard_link(stage, target)?;
    if let Err(error) = std::fs::remove_file(stage) {
        eprintln!(
            "write_file created {} but could not remove staging link {}: {error}",
            target.display(),
            stage.display()
        );
    }
    Ok(())
}

fn create_missing_file(req: &WriteFileRequest, path: &Path) -> WriteFileResult {
    if req.start_line != 1 || req.end_line != 1 {
        return failure_result(
            req,
            path,
            WriteFileStatus::RangeOutOfBounds,
            "A missing file can only be created with the line range 1-1",
        );
    }

    let Some(parent) = path.parent() else {
        return failure_result(
            req,
            path,
            WriteFileStatus::ParentNotFound,
            "The target path has no parent directory",
        );
    };
    let parent_metadata = match std::fs::metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return failure_result(
                req,
                path,
                WriteFileStatus::ParentNotFound,
                "The parent directory does not exist",
            );
        }
        Err(error) => {
            return io_failure(req, path, error, WriteFileStatus::WriteFailed);
        }
    };
    if !parent_metadata.is_dir() {
        return failure_result(
            req,
            path,
            WriteFileStatus::ParentNotADirectory,
            "The parent path is not a directory",
        );
    }

    let mut stage = match create_staged_file(path) {
        Ok(stage) => stage,
        Err(error) => return io_failure(req, path, error, WriteFileStatus::WriteFailed),
    };
    let write_result = (|| -> std::io::Result<()> {
        let file = stage.file.as_mut().expect("staging file should be open");
        file.write_all(req.text.as_bytes())?;
        file.flush()?;
        stage.close()
    })();
    if let Err(error) = write_result {
        return io_failure(req, path, error, WriteFileStatus::WriteFailed);
    }

    if let Err(error) = commit_creation(&stage.path, path) {
        let status = if error.kind() == std::io::ErrorKind::PermissionDenied {
            WriteFileStatus::AccessDenied
        } else {
            WriteFileStatus::ReplaceFailed
        };
        return failure_result(req, path, status, error.to_string());
    }
    stage.mark_committed();

    WriteFileResult {
        status: WriteFileStatus::Created,
        error: None,
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        replaced_line_count: None,
        inserted_bytes: req.text.len() as u64,
    }
}

fn replace_existing_file(
    req: &WriteFileRequest,
    path: &Path,
    target_path: &Path,
    pathname_metadata: std::fs::Metadata,
) -> WriteFileResult {
    let (file, opened_metadata) =
        match open_existing_regular_file(req, path, target_path, pathname_metadata) {
            Ok(opened) => opened,
            Err(result) => return result,
        };

    let mut stage = match create_staged_file(target_path) {
        Ok(stage) => stage,
        Err(error) => return io_failure(req, path, error, WriteFileStatus::WriteFailed),
    };
    let mut reader = std::io::BufReader::new(file);

    let edit_result = (|| -> Result<(), WriteFileResult> {
        let stage_file = stage.file.as_mut().expect("staging file should be open");
        let mut writer = std::io::BufWriter::new(stage_file);

        for _ in 1..req.start_line {
            match copy_line_exact(&mut reader, &mut writer) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(failure_result(
                        req,
                        path,
                        WriteFileStatus::RangeOutOfBounds,
                        "The requested line range extends beyond the end of the file",
                    ));
                }
                Err(error) => return Err(exact_copy_failure(req, path, error)),
            }
        }

        let mut preserved_bom = false;
        if req.start_line == 1 {
            let buffer = reader
                .fill_buf()
                .map_err(|error| io_failure(req, path, error, WriteFileStatus::ReadFailed))?;
            if buffer.starts_with(&[0xEF, 0xBB, 0xBF]) {
                reader.consume(3);
                preserved_bom = true;
            }
        }

        let empty_file_virtual_line =
            opened_metadata.len() == 0 && req.start_line == 1 && req.end_line == 1;
        let mut selected_terminator = None;
        for line_number in req.start_line..=req.end_line {
            let initial_bytes = preserved_bom && line_number == 1;
            let skipped = skip_line_with_terminator(&mut reader, initial_bytes)
                .map_err(|error| io_failure(req, path, error, WriteFileStatus::ReadFailed))?;
            if !(skipped.exists || empty_file_virtual_line && line_number == 1) {
                return Err(failure_result(
                    req,
                    path,
                    WriteFileStatus::RangeOutOfBounds,
                    "The requested line range extends beyond the end of the file",
                ));
            }
            if line_number == req.end_line {
                selected_terminator = skipped.terminator;
            }
        }

        let suffix_exists = !reader
            .fill_buf()
            .map_err(|error| io_failure(req, path, error, WriteFileStatus::ReadFailed))?
            .is_empty();

        if preserved_bom {
            writer
                .write_all(&[0xEF, 0xBB, 0xBF])
                .map_err(|error| io_failure(req, path, error, WriteFileStatus::WriteFailed))?;
        }
        writer
            .write_all(req.text.as_bytes())
            .map_err(|error| io_failure(req, path, error, WriteFileStatus::WriteFailed))?;
        if suffix_exists && !req.text.is_empty() && !req.text.as_bytes().ends_with(b"\n") {
            writer
                .write_all(selected_terminator.unwrap_or(b"\n"))
                .map_err(|error| io_failure(req, path, error, WriteFileStatus::WriteFailed))?;
        }
        copy_remaining_exact(&mut reader, &mut writer)
            .map_err(|error| exact_copy_failure(req, path, error))?;
        writer
            .flush()
            .map_err(|error| io_failure(req, path, error, WriteFileStatus::WriteFailed))?;
        Ok(())
    })();

    drop(reader);
    if let Err(result) = edit_result {
        return result;
    }
    if let Err(error) = stage.close() {
        return io_failure(req, path, error, WriteFileStatus::WriteFailed);
    }
    if let Err(error) = std::fs::set_permissions(&stage.path, opened_metadata.permissions()) {
        return io_failure(req, path, error, WriteFileStatus::WriteFailed);
    }
    if let Err(error) = commit_replacement(&stage.path, target_path) {
        let status = if error.kind() == std::io::ErrorKind::PermissionDenied {
            WriteFileStatus::AccessDenied
        } else {
            WriteFileStatus::ReplaceFailed
        };
        return failure_result(req, path, status, error.to_string());
    }
    stage.mark_committed();

    WriteFileResult {
        status: WriteFileStatus::Completed,
        error: None,
        path: path.to_string_lossy().into_owned(),
        requested_start_line: req.start_line,
        requested_end_line: req.end_line,
        replaced_line_count: Some(req.end_line - req.start_line + 1),
        inserted_bytes: req.text.len() as u64,
    }
}

fn handle_missing_target(
    req: &WriteFileRequest,
    path: &Path,
    original_error: std::io::Error,
) -> WriteFileResult {
    let Some(parent) = path.parent() else {
        return failure_result(
            req,
            path,
            WriteFileStatus::ParentNotFound,
            "The target path has no parent directory",
        );
    };

    match std::fs::metadata(parent) {
        Ok(metadata) if !metadata.is_dir() => failure_result(
            req,
            path,
            WriteFileStatus::ParentNotADirectory,
            "The parent path is not a directory",
        ),
        Ok(_) if req.create_if_missing => create_missing_file(req, path),
        Ok(_) => io_failure(req, path, original_error, WriteFileStatus::ReadFailed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => failure_result(
            req,
            path,
            WriteFileStatus::ParentNotFound,
            "The parent directory does not exist",
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => failure_result(
            req,
            path,
            WriteFileStatus::ParentNotADirectory,
            "A parent path component is not a directory",
        ),
        Err(error) => io_failure(req, path, error, WriteFileStatus::ReadFailed),
    }
}

fn write_file_blocking(req: WriteFileRequest, path: PathBuf) -> WriteFileResult {
    #[cfg(test)]
    test_hooks::wait_if_installed(&path);

    match std::fs::metadata(&path) {
        Ok(_) => {
            let target_path = match std::fs::canonicalize(&path) {
                Ok(target_path) => target_path,
                Err(error) => {
                    return io_failure(&req, &path, error, WriteFileStatus::ReadFailed);
                }
            };
            let target_metadata = match std::fs::metadata(&target_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return io_failure(&req, &path, error, WriteFileStatus::ReadFailed);
                }
            };
            replace_existing_file(&req, &path, &target_path, target_metadata)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            handle_missing_target(&req, &path, error)
        }
        Err(error) => io_failure(&req, &path, error, WriteFileStatus::ReadFailed),
    }
}

pub(crate) fn write_file_summary(result: &WriteFileResult) -> String {
    match result.status {
        WriteFileStatus::Completed if result.inserted_bytes == 0 => format!(
            "Deleted lines {}-{} from {}.",
            result.requested_start_line, result.requested_end_line, result.path
        ),
        WriteFileStatus::Completed => format!(
            "Replaced lines {}-{} in {}.",
            result.requested_start_line, result.requested_end_line, result.path
        ),
        WriteFileStatus::Created => format!("Created {}.", result.path),
        WriteFileStatus::NotFound => format!("File not found: {}.", result.path),
        WriteFileStatus::ParentNotFound => {
            format!("Parent directory not found for {}.", result.path)
        }
        WriteFileStatus::ParentNotADirectory => {
            format!("Parent path is not a directory for {}.", result.path)
        }
        WriteFileStatus::AccessDenied => format!("Access denied writing {}.", result.path),
        WriteFileStatus::NotAFile => {
            format!("Path is not a regular file: {}.", result.path)
        }
        WriteFileStatus::RangeOutOfBounds => format!(
            "Line range {}-{} is outside {}.",
            result.requested_start_line, result.requested_end_line, result.path
        ),
        WriteFileStatus::ReadFailed => format!("Reading {} for editing failed.", result.path),
        WriteFileStatus::WriteFailed => format!("Writing {} failed.", result.path),
        WriteFileStatus::ReplaceFailed => {
            format!("Committing the replacement for {} failed.", result.path)
        }
    }
}

impl McpServer {
    pub async fn write_file_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<WriteFileRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req = params.0;
        let replacement_bytes = req.text.len() as u64;
        let id = self.start_request(RequestData::WriteFile {
            path: req.path.clone(),
            start_line: req.start_line,
            end_line: req.end_line,
            replacement_bytes,
            create_if_missing: req.create_if_missing,
        });
        let path = match validate_write_file_request(&req) {
            Ok(path) => path,
            Err(error) => {
                self.update_request(
                    id,
                    RequestUpdate::Rejected {
                        error: error.clone(),
                    },
                );
                return Err(rmcp::ErrorData::invalid_params(error, None));
            }
        };

        let fallback_req = req.clone();
        let fallback_path = path.clone();
        let result = match tokio::task::spawn_blocking(move || write_file_blocking(req, path)).await
        {
            Ok(result) => result,
            Err(error) => failure_result(
                &fallback_req,
                &fallback_path,
                WriteFileStatus::WriteFailed,
                format!("Blocking file-write task failed: {error}"),
            ),
        };

        let update = RequestUpdate::WriteFileResponded {
            status: result.status,
            error: result.error.clone(),
            replaced_line_count: result.replaced_line_count,
            inserted_bytes: result.inserted_bytes,
        };
        let summary = write_file_summary(&result);
        self.finish_structured_request(id, summary, &result, update)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "remote_control_mcp_write_{prefix}_{}_{}",
            std::process::id(),
            id
        ))
    }

    fn request(path: &Path, start_line: u64, end_line: u64, text: &str) -> WriteFileRequest {
        WriteFileRequest {
            path: path.to_string_lossy().into_owned(),
            start_line,
            end_line,
            text: text.to_string(),
            create_if_missing: false,
        }
    }

    #[cfg(target_os = "windows")]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(not(target_os = "windows"))]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[test]
    fn replaces_lines_without_preserving_line_count_and_keeps_untouched_bytes() {
        let path = temp_path("replace");
        std::fs::write(&path, b"\xEF\xBB\xBFone\r\ntwo\r\n\xFFthree").unwrap();

        let result = write_file_blocking(request(&path, 2, 2, "new\nextra"), path.clone());

        assert_eq!(result.status, WriteFileStatus::Completed);
        assert_eq!(result.replaced_line_count, Some(1));
        assert_eq!(result.inserted_bytes, 9);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xEF\xBB\xBFone\r\nnew\nextra\r\n\xFFthree"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn replacement_of_first_line_preserves_bom_and_deletion_joins_cleanly() {
        let path = temp_path("bom_delete");
        std::fs::write(&path, b"\xEF\xBB\xBFone\ntwo\nthree\n").unwrap();

        let first = write_file_blocking(request(&path, 1, 1, "first"), path.clone());
        assert_eq!(first.status, WriteFileStatus::Completed);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xEF\xBB\xBFfirst\ntwo\nthree\n"
        );

        let deleted = write_file_blocking(request(&path, 2, 2, ""), path.clone());
        assert_eq!(deleted.status, WriteFileStatus::Completed);
        assert_eq!(std::fs::read(&path).unwrap(), b"\xEF\xBB\xBFfirst\nthree\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn final_line_replacement_controls_final_newline_exactly() {
        let path = temp_path("final");
        std::fs::write(&path, b"one\ntwo\n").unwrap();

        let result = write_file_blocking(request(&path, 2, 2, "last"), path.clone());

        assert_eq!(result.status, WriteFileStatus::Completed);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\nlast");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn strict_out_of_range_failure_leaves_original_unchanged() {
        let path = temp_path("range");
        let original = b"one\ntwo\n";
        std::fs::write(&path, original).unwrap();

        let result = write_file_blocking(request(&path, 2, 3, "replacement"), path.clone());

        assert_eq!(result.status, WriteFileStatus::RangeOutOfBounds);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_file_has_one_virtual_editable_line() {
        let path = temp_path("empty");
        std::fs::write(&path, b"").unwrap();

        let result = write_file_blocking(request(&path, 1, 1, "content"), path.clone());

        assert_eq!(result.status, WriteFileStatus::Completed);
        assert_eq!(std::fs::read(&path).unwrap(), b"content");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creation_commit_never_overwrites_a_concurrently_created_target() {
        let stage = temp_path("creation_stage");
        let target = temp_path("creation_target");
        std::fs::write(&stage, b"staged").unwrap();
        std::fs::write(&target, b"existing").unwrap();

        assert!(commit_creation(&stage, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        assert_eq!(std::fs::read(&stage).unwrap(), b"staged");

        std::fs::remove_file(stage).unwrap();
        std::fs::remove_file(target).unwrap();
    }

    #[test]
    fn missing_file_creation_is_explicit_and_requires_range_one_one() {
        let path = temp_path("create");
        let mut req = request(&path, 1, 1, "created");
        req.create_if_missing = true;

        let result = write_file_blocking(req, path.clone());

        assert_eq!(result.status, WriteFileStatus::Created);
        assert_eq!(result.replaced_line_count, None);
        assert_eq!(std::fs::read(&path).unwrap(), b"created");
        std::fs::remove_file(&path).unwrap();

        let mut invalid_range = request(&path, 2, 2, "no");
        invalid_range.create_if_missing = true;
        let invalid = write_file_blocking(invalid_range, path.clone());
        assert_eq!(invalid.status, WriteFileStatus::RangeOutOfBounds);
        assert!(!path.exists());
    }

    #[test]
    fn missing_parent_is_reported_without_creating_directories() {
        let parent = temp_path("missing_parent");
        let path = parent.join("file.txt");
        let mut req = request(&path, 1, 1, "content");
        req.create_if_missing = true;

        let result = write_file_blocking(req, path.clone());

        assert_eq!(result.status, WriteFileStatus::ParentNotFound);
        assert!(!parent.exists());
    }

    #[test]
    fn non_directory_parent_is_reported_consistently() {
        let parent = temp_path("parent_file");
        std::fs::write(&parent, b"not a directory").unwrap();
        let path = parent.join("child.txt");

        for create_if_missing in [false, true] {
            let mut req = request(&path, 1, 1, "content");
            req.create_if_missing = create_if_missing;
            let result = write_file_blocking(req, path.clone());
            assert_eq!(result.status, WriteFileStatus::ParentNotADirectory);
        }

        std::fs::remove_file(parent).unwrap();
    }

    #[test]
    fn exact_copy_failures_keep_read_and_write_statuses_distinct() {
        let path = temp_path("copy_failure");
        let req = request(&path, 1, 1, "content");

        let read = exact_copy_failure(
            &req,
            &path,
            ExactCopyError::Read(std::io::Error::other("injected read failure")),
        );
        let write = exact_copy_failure(
            &req,
            &path,
            ExactCopyError::Write(std::io::Error::other("injected write failure")),
        );

        assert_eq!(read.status, WriteFileStatus::ReadFailed);
        assert_eq!(write.status, WriteFileStatus::WriteFailed);
    }

    #[test]
    fn writes_through_symlinks_without_replacing_the_link() {
        let target = temp_path("symlink_target");
        let link = temp_path("symlink_link");
        std::fs::write(&target, b"one\ntwo\n").unwrap();
        if let Err(error) = create_file_symlink(&target, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                std::fs::remove_file(target).unwrap();
                return;
            }
            panic!("failed to create test symlink: {error}");
        }

        let result = write_file_blocking(request(&link, 2, 2, "changed"), link.clone());

        assert_eq!(result.status, WriteFileStatus::Completed);
        assert_eq!(std::fs::read(&target).unwrap(), b"one\nchanged");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(target).unwrap();
    }

    #[test]
    fn validation_reuses_read_file_ranges_and_limits_replacement_bytes() {
        let path = temp_path("validation");
        let mut req = request(&path, 1, 500, "");
        assert!(validate_write_file_request(&req).is_ok());

        req.end_line = 501;
        assert!(
            validate_write_file_request(&req)
                .unwrap_err()
                .contains("500")
        );

        req.start_line = u64::MAX;
        req.end_line = u64::MAX;
        assert!(validate_write_file_request(&req).is_ok());

        req.start_line = 1;
        req.end_line = 1;
        req.text = "x".repeat(MAX_REPLACEMENT_BYTES + 1);
        assert!(
            validate_write_file_request(&req)
                .unwrap_err()
                .contains("262144")
        );
    }
}
