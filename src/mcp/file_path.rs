use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "windows")]
use std::path::Prefix;

pub(crate) const MAX_REQUESTED_LINES: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegularFileOpenErrorKind {
    NotFound,
    AccessDenied,
    NotAFile,
    Other,
}

#[derive(Debug)]
pub(crate) struct RegularFileOpenError {
    pub(crate) kind: RegularFileOpenErrorKind,
    pub(crate) message: String,
}

fn io_error_kind(error: &std::io::Error) -> RegularFileOpenErrorKind {
    match error.kind() {
        std::io::ErrorKind::NotFound => RegularFileOpenErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => RegularFileOpenErrorKind::AccessDenied,
        _ => RegularFileOpenErrorKind::Other,
    }
}

fn io_open_error(error: std::io::Error) -> RegularFileOpenError {
    RegularFileOpenError {
        kind: io_error_kind(&error),
        message: error.to_string(),
    }
}

fn not_a_file(message: &str) -> RegularFileOpenError {
    RegularFileOpenError {
        kind: RegularFileOpenErrorKind::NotAFile,
        message: message.to_string(),
    }
}

pub(crate) fn validate_file_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path cannot contain null characters".to_string());
    }

    let requested_path = Path::new(path);
    if !requested_path.is_absolute()
        && matches!(
            requested_path.components().next(),
            Some(Component::Prefix(_) | Component::RootDir)
        )
    {
        return Err("path must be fully qualified or an ordinary relative path".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        if path.starts_with('\\') && !path.starts_with("\\\\") {
            return Err("root-relative Windows paths are not supported".to_string());
        }
        if matches!(
            requested_path.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::DeviceNS(_))
        ) {
            return Err("Windows device-namespace paths are not supported".to_string());
        }
    }

    let resolved = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        std::env::temp_dir().join(requested_path)
    };
    std::path::absolute(resolved)
        .map_err(|error| format!("path could not be resolved to an absolute path: {error}"))
}

pub(crate) fn open_regular_file_with_metadata(
    path: &Path,
    opened_metadata: impl FnOnce(&std::fs::File) -> std::io::Result<std::fs::Metadata>,
) -> Result<(std::fs::File, std::fs::Metadata), RegularFileOpenError> {
    let metadata = std::fs::metadata(path).map_err(io_open_error)?;
    if !metadata.is_file() {
        return Err(not_a_file("The resolved path is not a regular file"));
    }

    // The pathname check gives useful early classification, while metadata from
    // the opened handle prevents reading a different non-file swapped in before open.
    let file = std::fs::File::open(path).map_err(io_open_error)?;
    let metadata = opened_metadata(&file).map_err(io_open_error)?;
    if !metadata.is_file() {
        return Err(not_a_file("The opened path is not a regular file"));
    }

    Ok((file, metadata))
}

pub(crate) fn validate_line_file_path(
    path: &str,
    start_line: u64,
    end_line: u64,
) -> Result<PathBuf, String> {
    if start_line == 0 {
        return Err("start_line must be at least 1".to_string());
    }
    if end_line == 0 {
        return Err("end_line must be at least 1".to_string());
    }
    if start_line > end_line {
        return Err("start_line must be less than or equal to end_line".to_string());
    }
    if end_line - start_line >= MAX_REQUESTED_LINES {
        return Err(format!(
            "requested line range cannot exceed {MAX_REQUESTED_LINES} lines"
        ));
    }

    validate_file_path(path)
}
