use crate::mcp::file_path::validate_line_file_path;
use crate::mcp::read_file::post_edit_snippet;
use crate::mcp::write_file::{
    ExactCopyError, MAX_REPLACEMENT_BYTES, commit_replacement, consume_line_matching,
    copy_line_exact, copy_remaining_exact, create_staged_file,
};
use crate::mcp::{
    InsertFilePosition, InsertFileRequest, InsertFileResult, InsertFileStatus, McpServer,
    RequestData, RequestTimeoutOutcome, RequestUpdate, argument_error_result,
    missing_argument_message,
};
use std::io::{BufRead, Seek, Write};
use std::path::{Path, PathBuf};

pub(crate) fn validate_insert_file_request(req: &InsertFileRequest) -> Result<PathBuf, String> {
    let path = validate_line_file_path(&req.path, req.line, req.line)?;
    if req.expected_anchor_text.len() > MAX_REPLACEMENT_BYTES {
        return Err(format!(
            "expected_anchor_text cannot exceed {MAX_REPLACEMENT_BYTES} UTF-8 bytes"
        ));
    }
    if req.expected_anchor_text.contains('\n') {
        return Err(
            "expected_anchor_text must contain exactly one logical line and cannot contain LF"
                .to_string(),
        );
    }
    if req.text.is_empty() {
        return Err("text cannot be empty".to_string());
    }
    if req.text.len() > MAX_REPLACEMENT_BYTES {
        return Err(format!(
            "text cannot exceed {MAX_REPLACEMENT_BYTES} UTF-8 bytes"
        ));
    }
    Ok(path)
}

fn failure_result(
    req: &InsertFileRequest,
    path: &Path,
    status: InsertFileStatus,
    error: impl Into<String>,
) -> InsertFileResult {
    InsertFileResult {
        status,
        error: Some(error.into()),
        path: path.to_string_lossy().into_owned(),
        requested_line: req.line,
        inserted_bytes: 0,
        post_edit_start_line: None,
        post_edit_end_line: None,
        post_edit_text: String::new(),
        post_edit_truncated: false,
    }
}

fn io_status(error: &std::io::Error, fallback: InsertFileStatus) -> InsertFileStatus {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            InsertFileStatus::NotFound
        }
        std::io::ErrorKind::PermissionDenied => InsertFileStatus::AccessDenied,
        _ => fallback,
    }
}

fn io_failure(
    req: &InsertFileRequest,
    path: &Path,
    error: std::io::Error,
    fallback: InsertFileStatus,
) -> InsertFileResult {
    failure_result(req, path, io_status(&error, fallback), error.to_string())
}

fn exact_copy_failure(
    req: &InsertFileRequest,
    path: &Path,
    error: ExactCopyError,
) -> InsertFileResult {
    match error {
        ExactCopyError::Read(error) => io_failure(req, path, error, InsertFileStatus::ReadFailed),
        ExactCopyError::Write(error) => io_failure(req, path, error, InsertFileStatus::WriteFailed),
    }
}

fn preserve_leading_bom(
    req: &InsertFileRequest,
    path: &Path,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<bool, Box<InsertFileResult>> {
    if req.line != 1 {
        return Ok(false);
    }
    let buffer = reader
        .fill_buf()
        .map_err(|error| Box::new(io_failure(req, path, error, InsertFileStatus::ReadFailed)))?;
    if buffer.starts_with(&[0xEF, 0xBB, 0xBF]) {
        writer.write_all(&[0xEF, 0xBB, 0xBF]).map_err(|error| {
            Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
        })?;
        reader.consume(3);
        return Ok(true);
    }
    Ok(false)
}

fn insert_existing_file(
    req: &InsertFileRequest,
    path: &Path,
    position: InsertFilePosition,
) -> InsertFileResult {
    let target_path = match std::fs::canonicalize(path) {
        Ok(target_path) => target_path,
        Err(error) => return io_failure(req, path, error, InsertFileStatus::ReadFailed),
    };
    let pathname_metadata = match std::fs::metadata(&target_path) {
        Ok(metadata) => metadata,
        Err(error) => return io_failure(req, path, error, InsertFileStatus::ReadFailed),
    };
    if !pathname_metadata.is_file() {
        return failure_result(
            req,
            path,
            InsertFileStatus::NotAFile,
            "The resolved path is not a regular file",
        );
    }

    let file = match std::fs::File::open(&target_path) {
        Ok(file) => file,
        Err(error) => return io_failure(req, path, error, InsertFileStatus::ReadFailed),
    };
    let opened_metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return failure_result(
                req,
                path,
                InsertFileStatus::NotAFile,
                "The opened path is not a regular file",
            );
        }
        Err(error) => return io_failure(req, path, error, InsertFileStatus::ReadFailed),
    };

    let mut stage = match create_staged_file(&target_path) {
        Ok(stage) => stage,
        Err(error) => return io_failure(req, path, error, InsertFileStatus::WriteFailed),
    };
    let mut reader = std::io::BufReader::new(file);

    let edit_result = (|| -> Result<(), Box<InsertFileResult>> {
        let stage_file = stage.file.as_mut().expect("staging file should be open");
        let mut writer = std::io::BufWriter::new(stage_file);

        for _ in 1..req.line {
            match copy_line_exact(&mut reader, &mut writer) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(Box::new(failure_result(
                        req,
                        path,
                        InsertFileStatus::RangeOutOfBounds,
                        "The insertion anchor line is beyond the end of the file",
                    )));
                }
                Err(error) => return Err(Box::new(exact_copy_failure(req, path, error))),
            }
        }

        let preserved_bom = preserve_leading_bom(req, path, &mut reader, &mut writer)?;
        let anchor_start = reader.stream_position().map_err(|error| {
            Box::new(io_failure(req, path, error, InsertFileStatus::ReadFailed))
        })?;
        let anchor = consume_line_matching(
            &mut reader,
            req.expected_anchor_text.as_bytes(),
            preserved_bom,
        )
        .map_err(|error| Box::new(io_failure(req, path, error, InsertFileStatus::ReadFailed)))?;
        if !anchor.exists {
            return Err(Box::new(failure_result(
                req,
                path,
                InsertFileStatus::RangeOutOfBounds,
                "The insertion anchor line is beyond the end of the file",
            )));
        }
        if !anchor.matches {
            return Err(Box::new(failure_result(
                req,
                path,
                InsertFileStatus::ContentMismatch,
                "The insertion anchor line does not match expected_anchor_text",
            )));
        }
        reader
            .seek(std::io::SeekFrom::Start(anchor_start))
            .map_err(|error| {
                Box::new(io_failure(req, path, error, InsertFileStatus::ReadFailed))
            })?;

        match position {
            InsertFilePosition::Before => {
                writer.write_all(req.text.as_bytes()).map_err(|error| {
                    Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
                })?;
                if !req.text.as_bytes().ends_with(b"\n") {
                    writer
                        .write_all(anchor.terminator.unwrap_or(b"\n"))
                        .map_err(|error| {
                            Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
                        })?;
                }
                copy_remaining_exact(&mut reader, &mut writer)
                    .map_err(|error| Box::new(exact_copy_failure(req, path, error)))?;
            }
            InsertFilePosition::After => {
                copy_line_exact(&mut reader, &mut writer)
                    .map_err(|error| Box::new(exact_copy_failure(req, path, error)))?;
                if anchor.terminator.is_none() {
                    writer.write_all(b"\n").map_err(|error| {
                        Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
                    })?;
                }
                writer.write_all(req.text.as_bytes()).map_err(|error| {
                    Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
                })?;
                let suffix_exists = !reader
                    .fill_buf()
                    .map_err(|error| {
                        Box::new(io_failure(req, path, error, InsertFileStatus::ReadFailed))
                    })?
                    .is_empty();
                if suffix_exists && !req.text.as_bytes().ends_with(b"\n") {
                    writer
                        .write_all(anchor.terminator.unwrap_or(b"\n"))
                        .map_err(|error| {
                            Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
                        })?;
                }
                copy_remaining_exact(&mut reader, &mut writer)
                    .map_err(|error| Box::new(exact_copy_failure(req, path, error)))?;
            }
        }
        writer.flush().map_err(|error| {
            Box::new(io_failure(req, path, error, InsertFileStatus::WriteFailed))
        })?;
        Ok(())
    })();

    drop(reader);
    if let Err(result) = edit_result {
        return *result;
    }
    if let Err(error) = stage.close() {
        return io_failure(req, path, error, InsertFileStatus::WriteFailed);
    }
    let focus_line = match position {
        InsertFilePosition::Before => req.line,
        InsertFilePosition::After => req.line.saturating_add(1),
    };
    let snippet = post_edit_snippet(&stage.path, focus_line);
    if let Err(error) = std::fs::set_permissions(&stage.path, opened_metadata.permissions()) {
        return io_failure(req, path, error, InsertFileStatus::WriteFailed);
    }
    if let Err(error) = commit_replacement(&stage.path, &target_path) {
        let status = if error.kind() == std::io::ErrorKind::PermissionDenied {
            InsertFileStatus::AccessDenied
        } else {
            InsertFileStatus::ReplaceFailed
        };
        return failure_result(req, path, status, error.to_string());
    }
    stage.mark_committed();

    InsertFileResult {
        status: InsertFileStatus::Completed,
        error: None,
        path: path.to_string_lossy().into_owned(),
        requested_line: req.line,
        inserted_bytes: req.text.len() as u64,
        post_edit_start_line: snippet.start_line,
        post_edit_end_line: snippet.end_line,
        post_edit_text: snippet.text,
        post_edit_truncated: snippet.truncated,
    }
}

fn insert_file_blocking(
    req: InsertFileRequest,
    path: PathBuf,
    position: InsertFilePosition,
) -> InsertFileResult {
    match std::fs::metadata(&path) {
        Ok(_) => insert_existing_file(&req, &path, position),
        Err(error) => io_failure(&req, &path, error, InsertFileStatus::ReadFailed),
    }
}

pub(crate) fn insert_file_summary(
    result: &InsertFileResult,
    position: InsertFilePosition,
) -> String {
    let relation = match position {
        InsertFilePosition::Before => "before",
        InsertFilePosition::After => "after",
    };
    match result.status {
        InsertFileStatus::Completed => format!(
            "Inserted text {relation} line {} in {}.",
            result.requested_line, result.path
        ),
        InsertFileStatus::NotFound => format!("File not found: {}.", result.path),
        InsertFileStatus::AccessDenied => format!("Access denied writing {}.", result.path),
        InsertFileStatus::NotAFile => {
            format!("Path is not a regular file: {}.", result.path)
        }
        InsertFileStatus::RangeOutOfBounds => format!(
            "Insertion anchor line {} is outside {}.",
            result.requested_line, result.path
        ),
        InsertFileStatus::ContentMismatch => format!(
            "Anchor line {} did not match expected_anchor_text in {}.",
            result.requested_line, result.path
        ),
        InsertFileStatus::ReadFailed => format!("Reading {} for insertion failed.", result.path),
        InsertFileStatus::WriteFailed => format!("Writing {} failed.", result.path),
        InsertFileStatus::ReplaceFailed => {
            format!("Committing the insertion for {} failed.", result.path)
        }
    }
}

impl McpServer {
    pub async fn insert_file_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
        position: InsertFilePosition,
        tool_name: &'static str,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req: InsertFileRequest =
            match rmcp::serde_json::from_value(rmcp::serde_json::Value::Object(params.0)) {
                Ok(req) => req,
                Err(error) => {
                    return Ok(argument_error_result(missing_argument_message(
                        &error,
                        &["path", "line", "expected_anchor_text", "text"],
                    )));
                }
            };
        let id = self.start_request(RequestData::InsertFile {
            path: req.path.clone(),
            line: req.line,
            position,
            insertion_bytes: req.text.len() as u64,
        });
        let path = match validate_insert_file_request(&req) {
            Ok(path) => path,
            Err(error) => {
                self.update_request(
                    id,
                    RequestUpdate::Rejected {
                        error: error.clone(),
                    },
                );
                return Ok(argument_error_result(error));
            }
        };

        self.run_request_with_timeout(
            id,
            tool_name,
            RequestTimeoutOutcome::WriteFileMayContinue,
            async {
                let fallback_req = req.clone();
                let fallback_path = path.clone();
                let result = match tokio::task::spawn_blocking(move || {
                    insert_file_blocking(req, path, position)
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) => failure_result(
                        &fallback_req,
                        &fallback_path,
                        InsertFileStatus::WriteFailed,
                        format!("Blocking file-insertion task failed: {error}"),
                    ),
                };
                let update = RequestUpdate::InsertFileResponded {
                    status: result.status,
                    error: result.error.clone(),
                    inserted_bytes: result.inserted_bytes,
                };
                let summary = insert_file_summary(&result, position);
                self.finish_structured_request(id, summary, &result, false, update)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "remote_control_mcp_insert_{prefix}_{}_{}",
            std::process::id(),
            id
        ))
    }

    fn request(
        path: &Path,
        line: u64,
        expected_anchor_text: &str,
        text: &str,
    ) -> InsertFileRequest {
        InsertFileRequest {
            path: path.to_string_lossy().into_owned(),
            line,
            expected_anchor_text: expected_anchor_text.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn inserts_before_and_after_without_copying_anchor_content() {
        let before_path = temp_path("before");
        std::fs::write(&before_path, b"alpha\nbravo\ncharlie\n").unwrap();
        let before = insert_file_blocking(
            request(&before_path, 2, "bravo", "NEW"),
            before_path.clone(),
            InsertFilePosition::Before,
        );
        assert_eq!(before.status, InsertFileStatus::Completed);
        assert_eq!(
            before.post_edit_text,
            "1: alpha\n2: NEW\n3: bravo\n4: charlie"
        );
        assert_eq!(
            std::fs::read(&before_path).unwrap(),
            b"alpha\nNEW\nbravo\ncharlie\n"
        );

        let after_path = temp_path("after");
        std::fs::write(&after_path, b"alpha\nbravo\ncharlie\n").unwrap();
        let after = insert_file_blocking(
            request(&after_path, 2, "bravo", "NEW"),
            after_path.clone(),
            InsertFilePosition::After,
        );
        assert_eq!(after.status, InsertFileStatus::Completed);
        assert_eq!(
            std::fs::read(&after_path).unwrap(),
            b"alpha\nbravo\nNEW\ncharlie\n"
        );

        std::fs::remove_file(before_path).unwrap();
        std::fs::remove_file(after_path).unwrap();
    }

    #[test]
    fn insertion_preserves_bom_crlf_and_final_newline_semantics() {
        let before_path = temp_path("bom_crlf");
        std::fs::write(&before_path, b"\xEF\xBB\xBFalpha\r\nbravo\r\n").unwrap();
        let before = insert_file_blocking(
            request(&before_path, 1, "alpha", "NEW"),
            before_path.clone(),
            InsertFilePosition::Before,
        );
        assert_eq!(before.status, InsertFileStatus::Completed);
        assert_eq!(
            std::fs::read(&before_path).unwrap(),
            b"\xEF\xBB\xBFNEW\r\nalpha\r\nbravo\r\n"
        );

        let final_path = temp_path("final");
        std::fs::write(&final_path, b"alpha\nbravo").unwrap();
        let after = insert_file_blocking(
            request(&final_path, 2, "bravo", "NEW"),
            final_path.clone(),
            InsertFilePosition::After,
        );
        assert_eq!(after.status, InsertFileStatus::Completed);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"alpha\nbravo\nNEW");

        std::fs::remove_file(before_path).unwrap();
        std::fs::remove_file(final_path).unwrap();
    }

    #[test]
    fn insertion_rejects_empty_text_missing_files_empty_files_and_stale_lines() {
        let path = temp_path("invalid");
        assert!(validate_insert_file_request(&request(&path, 1, "", "")).is_err());
        assert!(validate_insert_file_request(&request(&path, 1, "one\ntwo", "NEW")).is_err());

        let missing = insert_file_blocking(
            request(&path, 1, "", "NEW"),
            path.clone(),
            InsertFilePosition::Before,
        );
        assert_eq!(missing.status, InsertFileStatus::NotFound);

        std::fs::write(&path, b"").unwrap();
        let empty = insert_file_blocking(
            request(&path, 1, "", "NEW"),
            path.clone(),
            InsertFilePosition::Before,
        );
        assert_eq!(empty.status, InsertFileStatus::RangeOutOfBounds);
        assert!(empty.post_edit_text.is_empty());

        std::fs::write(&path, b"one\n").unwrap();
        let stale = insert_file_blocking(
            request(&path, 2, "", "NEW"),
            path.clone(),
            InsertFilePosition::After,
        );
        assert_eq!(stale.status, InsertFileStatus::RangeOutOfBounds);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\n");

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn insertion_rejects_a_stale_anchor_without_changing_the_file() {
        let path = temp_path("stale_anchor");
        std::fs::write(&path, b"alpha\nbravo\ncharlie\ndelta\n").unwrap();

        let shift = insert_file_blocking(
            request(&path, 2, "bravo", "SHIFT"),
            path.clone(),
            InsertFilePosition::Before,
        );
        assert_eq!(shift.status, InsertFileStatus::Completed);

        let stale = insert_file_blocking(
            request(&path, 3, "charlie", "STALE"),
            path.clone(),
            InsertFilePosition::After,
        );
        assert_eq!(stale.status, InsertFileStatus::ContentMismatch);
        assert_eq!(stale.inserted_bytes, 0);
        assert!(stale.post_edit_text.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"alpha\nSHIFT\nbravo\ncharlie\ndelta\n"
        );

        std::fs::remove_file(path).unwrap();
    }
}
