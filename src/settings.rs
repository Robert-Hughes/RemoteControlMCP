use std::fs;
use std::path::{Path, PathBuf};

const MAXIMUM_REQUEST_TIMEOUT_FILE: &str = "maximum-request-timeout.txt";
pub const DEFAULT_MAXIMUM_REQUEST_TIMEOUT_SECONDS: u64 = 0;

pub fn load_maximum_request_timeout_seconds() -> Result<u64, String> {
    load_maximum_request_timeout_seconds_from(&config_directory()?)
}

pub fn save_maximum_request_timeout_seconds(seconds: u64) -> Result<(), String> {
    save_maximum_request_timeout_seconds_to(&config_directory()?, seconds)
}

fn load_maximum_request_timeout_seconds_from(config_dir: &Path) -> Result<u64, String> {
    let path = application_config_directory(config_dir).join(MAXIMUM_REQUEST_TIMEOUT_FILE);
    let value = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DEFAULT_MAXIMUM_REQUEST_TIMEOUT_SECONDS);
        }
        Err(error) => {
            return Err(format!(
                "Could not read the maximum request timeout setting {}: {error}",
                path.display()
            ));
        }
    };

    value.trim().parse::<u64>().map_err(|error| {
        format!(
            "The maximum request timeout setting {} must contain a non-negative whole number of seconds: {error}",
            path.display()
        )
    })
}

fn save_maximum_request_timeout_seconds_to(config_dir: &Path, seconds: u64) -> Result<(), String> {
    let directory = application_config_directory(config_dir);
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Could not create the application configuration directory {}: {error}",
            directory.display()
        )
    })?;
    let path = directory.join(MAXIMUM_REQUEST_TIMEOUT_FILE);
    fs::write(&path, format!("{seconds}\n")).map_err(|error| {
        format!(
            "Could not save the maximum request timeout setting {}: {error}",
            path.display()
        )
    })
}

fn application_config_directory(config_dir: &Path) -> PathBuf {
    config_dir.join("RemoteControlMCP")
}

fn config_directory() -> Result<PathBuf, String> {
    if let Some(xdg_config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config"));
    }
    dirs::config_dir().ok_or_else(|| {
        "The user configuration directory could not be determined, so the application settings cannot be located.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_config_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rmcp-settings-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn maximum_request_timeout_defaults_to_zero_and_round_trips() {
        let config_dir = test_config_dir();
        assert_eq!(
            load_maximum_request_timeout_seconds_from(&config_dir).unwrap(),
            0
        );
        save_maximum_request_timeout_seconds_to(&config_dir, 110).unwrap();
        assert_eq!(
            load_maximum_request_timeout_seconds_from(&config_dir).unwrap(),
            110
        );
        save_maximum_request_timeout_seconds_to(&config_dir, 0).unwrap();
        assert_eq!(
            load_maximum_request_timeout_seconds_from(&config_dir).unwrap(),
            0
        );
        fs::remove_dir_all(config_dir).unwrap();
    }

    #[test]
    fn maximum_request_timeout_rejects_invalid_text() {
        let config_dir = test_config_dir();
        let directory = application_config_directory(&config_dir);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MAXIMUM_REQUEST_TIMEOUT_FILE), "1.5\n").unwrap();
        let error = load_maximum_request_timeout_seconds_from(&config_dir).unwrap_err();
        assert!(error.contains("non-negative whole number of seconds"));
        fs::remove_dir_all(config_dir).unwrap();
    }
}
