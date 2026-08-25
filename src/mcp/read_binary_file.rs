use crate::mcp::file_path::{
    RegularFileOpenError, RegularFileOpenErrorKind, open_regular_file_with_metadata,
    validate_file_path,
};
use crate::mcp::{
    McpServer, ReadBinaryFileContentKind, ReadBinaryFileRequest, ReadBinaryFileResult,
    ReadBinaryFileStatus, RequestData, RequestTimeoutOutcome, RequestUpdate, argument_error_result,
    missing_argument_message,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use rmcp::model::{ContentBlock, ResourceContents};
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) const MAX_BINARY_FILE_BYTES: u64 = 100_000_000;

struct BinaryReadOutcome {
    result: ReadBinaryFileResult,
    bytes: Option<Vec<u8>>,
}

pub(crate) fn validate_read_binary_file_request(
    req: &ReadBinaryFileRequest,
) -> Result<PathBuf, String> {
    if let Some(max_bytes) = req.max_bytes {
        if max_bytes == 0 {
            return Err("max_bytes must be at least 1".to_string());
        }
        if max_bytes > MAX_BINARY_FILE_BYTES {
            return Err(format!(
                "max_bytes cannot exceed the server hard limit of {MAX_BINARY_FILE_BYTES} bytes"
            ));
        }
    }
    validate_file_path(&req.path)
}

fn effective_limit(req: &ReadBinaryFileRequest) -> u64 {
    req.max_bytes.unwrap_or(MAX_BINARY_FILE_BYTES)
}

fn failure_result(
    path: &Path,
    status: ReadBinaryFileStatus,
    error: impl Into<String>,
    size: Option<u64>,
) -> BinaryReadOutcome {
    BinaryReadOutcome {
        result: ReadBinaryFileResult {
            status,
            error: Some(error.into()),
            path: path.to_string_lossy().into_owned(),
            size,
            mime_type: None,
            content_kind: None,
        },
        bytes: None,
    }
}

fn open_failure(path: &Path, error: RegularFileOpenError) -> BinaryReadOutcome {
    let status = match error.kind {
        RegularFileOpenErrorKind::NotFound => ReadBinaryFileStatus::NotFound,
        RegularFileOpenErrorKind::AccessDenied => ReadBinaryFileStatus::AccessDenied,
        RegularFileOpenErrorKind::NotAFile => ReadBinaryFileStatus::NotAFile,
        RegularFileOpenErrorKind::Other => ReadBinaryFileStatus::ReadFailed,
    };
    failure_result(path, status, error.message, None)
}

fn read_failure(path: &Path, error: std::io::Error) -> BinaryReadOutcome {
    let status = if error.kind() == std::io::ErrorKind::PermissionDenied {
        ReadBinaryFileStatus::AccessDenied
    } else {
        ReadBinaryFileStatus::ReadFailed
    };
    failure_result(path, status, error.to_string(), None)
}

fn extension_mime(path: &Path) -> Option<(&'static str, ReadBinaryFileContentKind)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let image = ReadBinaryFileContentKind::Image;
    let resource = ReadBinaryFileContentKind::EmbeddedResource;
    match extension.as_str() {
        "png" => Some(("image/png", image)),
        "jpg" | "jpeg" => Some(("image/jpeg", image)),
        "webp" => Some(("image/webp", image)),
        "gif" => Some(("image/gif", image)),
        "bmp" => Some(("image/bmp", image)),
        "tif" | "tiff" => Some(("image/tiff", image)),
        "avif" => Some(("image/avif", image)),
        "pdf" => Some(("application/pdf", resource)),
        "zip" => Some(("application/zip", resource)),
        "gz" => Some(("application/gzip", resource)),
        "7z" => Some(("application/x-7z-compressed", resource)),
        "mp3" => Some(("audio/mpeg", resource)),
        "wav" => Some(("audio/wav", resource)),
        "mp4" => Some(("video/mp4", resource)),
        _ => None,
    }
}

fn magic_mime(bytes: &[u8]) -> Option<(&'static str, ReadBinaryFileContentKind)> {
    let image = ReadBinaryFileContentKind::Image;
    let resource = ReadBinaryFileContentKind::EmbeddedResource;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", image))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("image/jpeg", image))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", image))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", image))
    } else if bytes.starts_with(b"%PDF-") {
        Some(("application/pdf", resource))
    } else {
        None
    }
}

fn detect_mime(path: &Path, bytes: &[u8]) -> (&'static str, ReadBinaryFileContentKind) {
    extension_mime(path)
        .or_else(|| magic_mime(bytes))
        .unwrap_or((
            "application/octet-stream",
            ReadBinaryFileContentKind::EmbeddedResource,
        ))
}

fn read_binary_file_blocking(req: ReadBinaryFileRequest, path: PathBuf) -> BinaryReadOutcome {
    let limit = effective_limit(&req);
    let (file, metadata) = match open_regular_file_with_metadata(&path, std::fs::File::metadata) {
        Ok(opened) => opened,
        Err(error) => return open_failure(&path, error),
    };

    if metadata.len() > limit {
        return failure_result(
            &path,
            ReadBinaryFileStatus::TooLarge,
            format!(
                "File is {} bytes; read_binary_file supports at most {limit} bytes for this request",
                metadata.len()
            ),
            Some(metadata.len()),
        );
    }

    let capacity = usize::try_from(metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    let mut reader = file.take(limit.saturating_add(1));
    if let Err(error) = reader.read_to_end(&mut bytes) {
        return read_failure(&path, error);
    }
    if bytes.len() as u64 > limit {
        return failure_result(
            &path,
            ReadBinaryFileStatus::TooLarge,
            format!(
                "File grew beyond the {limit}-byte read_binary_file limit while it was being read"
            ),
            None,
        );
    }

    let (mime_type, content_kind) = detect_mime(&path, &bytes);
    BinaryReadOutcome {
        result: ReadBinaryFileResult {
            status: ReadBinaryFileStatus::Completed,
            error: None,
            path: path.to_string_lossy().into_owned(),
            size: Some(bytes.len() as u64),
            mime_type: Some(mime_type.to_string()),
            content_kind: Some(content_kind),
        },
        bytes: Some(bytes),
    }
}

fn percent_encode_uri_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn file_resource_uri(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_uri_path(&normalized);
    if cfg!(target_os = "windows") {
        if encoded.starts_with("//") {
            format!("file:{encoded}")
        } else {
            format!("file:///{}", encoded.trim_start_matches('/'))
        }
    } else {
        format!("file://{encoded}")
    }
}

pub(crate) fn read_binary_file_summary(result: &ReadBinaryFileResult) -> String {
    match result.status {
        ReadBinaryFileStatus::Completed => format!(
            "Read {} bytes from {} as {}.",
            result.size.unwrap_or(0),
            result.path,
            result
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream")
        ),
        ReadBinaryFileStatus::NotFound => format!("File not found: {}.", result.path),
        ReadBinaryFileStatus::AccessDenied => format!("Access denied reading {}.", result.path),
        ReadBinaryFileStatus::NotAFile => format!("Path is not a regular file: {}.", result.path),
        ReadBinaryFileStatus::TooLarge => {
            format!("Binary file is too large to read: {}.", result.path)
        }
        ReadBinaryFileStatus::ReadFailed => format!("Reading {} failed.", result.path),
    }
}

impl McpServer {
    pub async fn read_binary_file_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req: ReadBinaryFileRequest =
            match rmcp::serde_json::from_value(rmcp::serde_json::Value::Object(params.0)) {
                Ok(req) => req,
                Err(error) => {
                    return Ok(argument_error_result(missing_argument_message(
                        &error,
                        &["path"],
                    )));
                }
            };
        let id = self.start_request(RequestData::ReadBinaryFile {
            path: req.path.clone(),
            max_bytes: req.max_bytes,
        });
        let path = match validate_read_binary_file_request(&req) {
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
            "read_binary_file",
            RequestTimeoutOutcome::ReadBinaryFileMayContinue,
            async {
                let fallback_path = path.clone();
                let outcome =
                    match tokio::task::spawn_blocking(move || read_binary_file_blocking(req, path))
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => failure_result(
                            &fallback_path,
                            ReadBinaryFileStatus::ReadFailed,
                            format!("Blocking binary file-read task failed: {error}"),
                            None,
                        ),
                    };

                let result = outcome.result;
                let update = RequestUpdate::ReadBinaryFileResponded {
                    status: result.status,
                    error: result.error.clone(),
                    size: result.size,
                    mime_type: result.mime_type.clone(),
                    content_kind: result.content_kind,
                };
                let summary = read_binary_file_summary(&result);
                let mut content = vec![ContentBlock::text(summary)];
                if let Some(bytes) = outcome.bytes {
                    let encoded = BASE64_STANDARD.encode(bytes);
                    match result.content_kind {
                        Some(ReadBinaryFileContentKind::Image) => {
                            content.push(ContentBlock::image(
                                encoded,
                                result
                                    .mime_type
                                    .as_deref()
                                    .unwrap_or("application/octet-stream"),
                            ))
                        }
                        Some(ReadBinaryFileContentKind::EmbeddedResource) => {
                            let resource =
                                ResourceContents::blob(encoded, file_resource_uri(&fallback_path))
                                    .with_mime_type(
                                        result
                                            .mime_type
                                            .as_deref()
                                            .unwrap_or("application/octet-stream"),
                                    );
                            content.push(ContentBlock::resource(resource));
                        }
                        None => {}
                    }
                }

                self.finish_structured_request_with_content(id, content, &result, false, update)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_detection_prefers_extension_and_falls_back_to_magic() {
        assert_eq!(
            detect_mime(Path::new("photo.PNG"), b"not-a-real-png"),
            ("image/png", ReadBinaryFileContentKind::Image)
        );
        assert_eq!(
            detect_mime(Path::new("no-extension"), b"\x89PNG\r\n\x1a\nrest"),
            ("image/png", ReadBinaryFileContentKind::Image)
        );
        assert_eq!(
            detect_mime(Path::new("unknown.bin"), b"\x00\x01\x02"),
            (
                "application/octet-stream",
                ReadBinaryFileContentKind::EmbeddedResource
            )
        );
    }

    #[test]
    fn resource_uri_percent_encodes_path_text() {
        let uri = file_resource_uri(Path::new("/tmp/a file#1.bin"));
        assert!(uri.starts_with("file:"));
        assert!(uri.contains("a%20file%231.bin"));
    }
}
