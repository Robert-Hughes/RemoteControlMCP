use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_REQUESTED_LINES: u64 = 500;

pub(crate) fn validate_line_file_path(
    path: &str,
    start_line: u64,
    end_line: u64,
) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path cannot contain null characters".to_string());
    }
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
    if path.starts_with('\\') && !path.starts_with("\\\\") {
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
