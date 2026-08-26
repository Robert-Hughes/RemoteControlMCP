use chrono::{Local, SecondsFormat};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const USAGE_LOG_FILE: &str = "tool-usage.jsonl";

#[derive(Clone)]
pub struct UsageLog {
    writer: Arc<Mutex<Option<BufWriter<File>>>>,
}

#[derive(Serialize)]
struct UsageRecord<'a> {
    timestamp: String,
    tool: &'a str,
}

impl UsageLog {
    pub fn open() -> Self {
        match usage_log_path().and_then(|path| open_writer(&path)) {
            Ok(writer) => Self::from_writer(Some(writer)),
            Err(error) => {
                eprintln!("Tool usage logging is disabled: {error}");
                Self::disabled()
            }
        }
    }

    pub fn record(&self, tool: &str) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(file_writer) = writer.as_mut() else {
            return;
        };
        let record = UsageRecord {
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Millis, false),
            tool,
        };
        let result = serde_json::to_writer(&mut *file_writer, &record)
            .and_then(|()| file_writer.write_all(b"\n").map_err(serde_json::Error::io))
            .and_then(|()| file_writer.flush().map_err(serde_json::Error::io));
        if let Err(error) = result {
            eprintln!("Tool usage logging failed and has been disabled: {error}");
            *writer = None;
        }
    }

    pub(crate) fn disabled() -> Self {
        Self::from_writer(None)
    }

    fn from_writer(writer: Option<BufWriter<File>>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    #[cfg(test)]
    pub(crate) fn open_at(path: &Path) -> Self {
        Self::from_writer(open_writer(path).ok())
    }
}

fn usage_log_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|directory| directory.join("RemoteControlMCP").join(USAGE_LOG_FILE))
        .ok_or_else(|| "the user-local data directory could not be determined".to_string())
}

fn open_writer(path: &Path) -> Result<BufWriter<File>, String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("usage log path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(directory).map_err(|error| {
        format!(
            "could not create tool usage log directory {}: {error}",
            directory.display()
        )
    })?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(BufWriter::new)
        .map_err(|error| format!("could not open tool usage log {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn writes_json_lines_without_corrupting_special_tool_names() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "remote-control-mcp-usage-test-{}-{}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let log = UsageLog::open_at(&path);
        log.record("read_flie\t\"unexpected\"\nnext");

        let contents = std::fs::read_to_string(&path).unwrap();
        let record: serde_json::Value = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(record["tool"], "read_flie\t\"unexpected\"\nnext");
        assert!(
            record["timestamp"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );

        std::fs::remove_file(path).unwrap();
    }
}
