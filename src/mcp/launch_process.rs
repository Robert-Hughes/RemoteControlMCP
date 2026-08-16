use crate::mcp::{
    LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer, RequestData,
    RequestId, RequestUpdate, TimeoutAction, UiEvent, UiEventKind, argument_error_result,
    missing_argument_message,
};
use std::sync::mpsc::Sender;
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;
const OUTPUT_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OutputProgressSnapshot {
    pub stdout_lines: u64,
    pub stderr_lines: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Default)]
struct StreamLineCounter {
    offset: u64,
    newline_count: u64,
    last_byte: Option<u8>,
    file_len: u64,
}

impl StreamLineCounter {
    fn refresh(&mut self, path: &str) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.newline_count = 0;
            self.last_byte = None;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            for byte in &buffer[..read] {
                if *byte == b'\n' {
                    self.newline_count = self.newline_count.saturating_add(1);
                }
                self.last_byte = Some(*byte);
            }
            self.offset = self.offset.saturating_add(read as u64);
        }
        self.file_len = len.max(self.offset);
        Ok(())
    }

    fn lines(&self) -> u64 {
        self.newline_count
            .saturating_add(u64::from(self.offset > 0 && self.last_byte != Some(b'\n')))
    }
}

#[derive(Default)]
struct OutputProgressState {
    stdout: StreamLineCounter,
    stderr: StreamLineCounter,
    last_sent: Option<OutputProgressSnapshot>,
}

struct OutputProgressShared {
    state: std::sync::Mutex<OutputProgressState>,
    stdout_path: String,
    stderr_path: String,
    max_output_bytes: usize,
    pid: u32,
    tx: Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
}

fn refresh_output_progress(
    state: &mut OutputProgressState,
    stdout_path: &str,
    stderr_path: &str,
    max_output_bytes: usize,
) -> OutputProgressSnapshot {
    let _ = state.stdout.refresh(stdout_path);
    let _ = state.stderr.refresh(stderr_path);
    OutputProgressSnapshot {
        stdout_lines: state.stdout.lines(),
        stderr_lines: state.stderr.lines(),
        stdout_truncated: state.stdout.file_len > max_output_bytes as u64,
        stderr_truncated: state.stderr.file_len > max_output_bytes as u64,
    }
}

fn emit_output_progress(shared: &OutputProgressShared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let progress = refresh_output_progress(
        &mut state,
        &shared.stdout_path,
        &shared.stderr_path,
        shared.max_output_bytes,
    );
    if state.last_sent == Some(progress) {
        return;
    }
    let _ = shared.tx.send(UiEvent {
        elapsed: shared.start_time.elapsed(),
        kind: UiEventKind::RequestUpdated {
            id: shared.request_id,
            update: RequestUpdate::LaunchProcessOutputProgress {
                pid: shared.pid,
                stdout_lines: progress.stdout_lines,
                stderr_lines: progress.stderr_lines,
                stdout_truncated: progress.stdout_truncated,
                stderr_truncated: progress.stderr_truncated,
            },
        },
    });
    state.last_sent = Some(progress);
}

#[derive(Clone)]
struct OutputProgressCompletion {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shared: std::sync::Arc<OutputProgressShared>,
}

impl OutputProgressCompletion {
    fn finish(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        emit_output_progress(&self.shared);
    }
}

struct OutputProgressMonitor {
    completion: OutputProgressCompletion,
    finish_on_drop: bool,
}

impl OutputProgressMonitor {
    fn completion_handle(&self) -> OutputProgressCompletion {
        self.completion.clone()
    }

    fn disarm(&mut self) {
        self.finish_on_drop = false;
    }
}

impl Drop for OutputProgressMonitor {
    fn drop(&mut self) {
        if self.finish_on_drop {
            self.completion.finish();
        }
    }
}

fn start_output_progress_monitor(
    stdout_path: String,
    stderr_path: String,
    max_output_bytes: usize,
    pid: u32,
    tx: Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
) -> OutputProgressMonitor {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = std::sync::Arc::new(OutputProgressShared {
        state: std::sync::Mutex::new(OutputProgressState::default()),
        stdout_path,
        stderr_path,
        max_output_bytes,
        pid,
        tx,
        start_time,
        request_id,
    });
    let monitor_stop = stop.clone();
    let monitor_shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name(format!("mcp-output-{pid}"))
        .spawn(move || {
            loop {
                emit_output_progress(&monitor_shared);
                if monitor_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(OUTPUT_PROGRESS_INTERVAL);
            }
        });
    OutputProgressMonitor {
        completion: OutputProgressCompletion { stop, shared },
        finish_on_drop: true,
    }
}

#[cfg(test)]
pub(crate) fn output_progress_snapshot_for_test(
    stdout_path: &str,
    stderr_path: &str,
    max_output_bytes: usize,
) -> OutputProgressSnapshot {
    let mut state = OutputProgressState::default();
    refresh_output_progress(&mut state, stdout_path, stderr_path, max_output_bytes)
}

fn generate_output_files() -> Result<(std::fs::File, std::fs::File, String, String), std::io::Error>
{
    let dir = std::env::temp_dir().join("RemoteControlMCP");
    std::fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let stdout_name = format!(
        "launch-process-{}-{}-{}.stdout.log",
        pid, timestamp, counter
    );
    let stderr_name = format!(
        "launch-process-{}-{}-{}.stderr.log",
        pid, timestamp, counter
    );

    let stdout_path = dir.join(stdout_name);
    let stderr_path = dir.join(stderr_name);

    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    let stdout_str = stdout_path.to_string_lossy().into_owned();
    let stderr_str = stderr_path.to_string_lossy().into_owned();

    Ok((stdout_file, stderr_file, stdout_str, stderr_str))
}

pub fn read_and_truncate_file(
    path: &str,
    max_output_bytes: usize,
) -> Result<String, std::io::Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();

    if len == 0 {
        return Ok(String::new());
    }

    let (to_read, truncated) = if len > max_output_bytes as u64 {
        (max_output_bytes, true)
    } else {
        (len as usize, false)
    };

    let mut buffer = vec![0u8; to_read];
    if truncated {
        file.seek(SeekFrom::Start(len - max_output_bytes as u64))?;
    }
    file.read_exact(&mut buffer)?;

    let decoded = String::from_utf8_lossy(&buffer).into_owned();
    if truncated {
        Ok(format!(
            "[... beginning truncated; full output available in {path} ...]\n{decoded}"
        ))
    } else {
        Ok(decoded)
    }
}

struct FinalOutput {
    error: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

fn read_final_output(stdout_path: &str, stderr_path: &str, max_output_bytes: usize) -> FinalOutput {
    let mut errors = Vec::new();
    let stdout = match read_and_truncate_file(stdout_path, max_output_bytes) {
        Ok(output) => Some(output),
        Err(error) => {
            errors.push(format!("Failed to read stdout: {error}"));
            None
        }
    };
    let stderr = match read_and_truncate_file(stderr_path, max_output_bytes) {
        Ok(output) => Some(output),
        Err(error) => {
            errors.push(format!("Failed to read stderr: {error}"));
            None
        }
    };

    FinalOutput {
        error: (!errors.is_empty()).then(|| errors.join(". ")),
        stdout,
        stderr,
    }
}

pub(crate) fn validate_request(req: &LaunchProcessRequest) -> Result<(), String> {
    if req.process_name.is_empty() {
        return Err("process_name cannot be empty".to_string());
    }
    if req.process_name.contains('\0') {
        return Err("process_name cannot contain null characters".to_string());
    }
    if req
        .working_directory
        .as_ref()
        .is_some_and(|dir| dir.contains('\0'))
    {
        return Err("working_directory cannot contain null characters".to_string());
    }
    if req
        .arguments
        .as_ref()
        .is_some_and(|arguments| arguments.iter().any(|argument| argument.contains('\0')))
    {
        return Err("arguments cannot contain null characters".to_string());
    }

    for (k, v) in &req.environment.variables {
        if k.is_empty() {
            return Err("environment variable name cannot be empty".to_string());
        }
        if k.contains('=') {
            return Err("environment variable name cannot contain '='".to_string());
        }
        if k.contains('\0') {
            return Err("environment variable name cannot contain null characters".to_string());
        }
        if v.as_ref().is_some_and(|val| val.contains('\0')) {
            return Err("environment variable value cannot contain null characters".to_string());
        }
    }

    if let Some(ms) = req.timeout_ms {
        if ms == 0 {
            return Err("timeout_ms must be greater than zero".to_string());
        }
        if req.timeout_action.is_none() {
            return Err("timeout_ms requires timeout_action".to_string());
        }
    }

    if let Some(ref action) = req.timeout_action {
        if req.timeout_ms.is_none() {
            return Err("timeout_action requires timeout_ms".to_string());
        }
        if req.detached && *action == TimeoutAction::Detach {
            return Err(
                "detached = true together with timeout_action = 'detach' is invalid".to_string(),
            );
        }
    }

    if let Some(max_output_bytes) = req.max_output_bytes {
        if max_output_bytes == 0 {
            return Err("max_output_bytes must be greater than zero".to_string());
        }
        if usize::try_from(max_output_bytes).is_err() {
            return Err("max_output_bytes is too large for this platform".to_string());
        }
    }

    Ok(())
}
fn command_line_for_display(req: &LaunchProcessRequest) -> String {
    let mut command_line = req.process_name.clone();

    if let Some(arguments) = req
        .arguments
        .as_deref()
        .filter(|arguments| !arguments.is_empty())
    {
        for argument in arguments {
            command_line.push(' ');
            command_line.push_str(argument);
        }
    }

    command_line
}
#[cfg(test)]
pub use crate::mcp::test_hooks;

pub(crate) trait ChildOps {
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus>;
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl ChildOps for std::process::Child {
    fn kill(&mut self) -> std::io::Result<()> {
        self.kill()
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.wait()
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.try_wait()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupOutcome {
    KillSucceeded,
    KillFailedChildExited,
    KillFailedChildRunning { reaper_started: bool },
    KillFailedStatusUnknown { reaper_started: bool },
    WaitFailedReaperStarted,
    WaitFailedReaperStartFailed,
}

#[derive(Debug)]
pub(crate) enum MonitorOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    WaitFailed(std::io::Error),
}

pub(crate) fn report_background_error(
    tx: &Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
    pid: u32,
    error: String,
) {
    eprintln!("Background process handling failed for PID {pid}: {error}");
    let _ = tx.send(UiEvent {
        elapsed: start_time.elapsed(),
        kind: UiEventKind::RequestUpdated {
            id: request_id,
            update: RequestUpdate::LaunchProcessBackgroundError { pid, error },
        },
    });
}

fn handle_background_wait_result(
    wait_result: std::io::Result<std::process::ExitStatus>,
    pid: u32,
    tx: &Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
    context: &str,
) {
    handle_background_wait_result_with_notifier(
        wait_result,
        pid,
        tx,
        start_time,
        request_id,
        context,
        |pid| {
            #[cfg(test)]
            test_hooks::notify_completion(pid);
            #[cfg(not(test))]
            let _ = pid;
        },
    );
}

pub(crate) fn handle_background_wait_result_with_notifier<F>(
    wait_result: std::io::Result<std::process::ExitStatus>,
    pid: u32,
    tx: &Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
    context: &str,
    notify_success: F,
) where
    F: FnOnce(u32),
{
    match wait_result {
        // Completion means the child was successfully waited on and reaped.
        Ok(_) => notify_success(pid),
        Err(error) => report_background_error(
            tx,
            start_time,
            request_id,
            pid,
            format!(
                "{context}: {error}. Successful reaping could not be confirmed; the process may remain running or unreaped"
            ),
        ),
    }
}

struct BackgroundReaperOptions {
    thread_name: String,
    context: &'static str,
    output_completion: Option<OutputProgressCompletion>,
}

fn spawn_background_reaper(
    child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>,
    pid: u32,
    tx: Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
    options: BackgroundReaperOptions,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(options.thread_name)
        .spawn(move || {
            let child = child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(mut child) = child {
                let wait_result = child.wait();
                handle_background_wait_result(
                    wait_result,
                    pid,
                    &tx,
                    start_time,
                    request_id,
                    options.context,
                );
            }
            if let Some(completion) = options.output_completion {
                completion.finish();
            }
        })
        .map(|_| ())
}
#[derive(Clone, Copy)]
struct BackgroundContext<'a> {
    tx: &'a Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
}
#[cfg(test)]
pub(crate) fn perform_cleanup<C, F>(
    child: C,
    pid: u32,
    original_error: &str,
    is_timeout_stop: bool,
    stdout_path: &str,
    stderr_path: &str,
    spawn_reaper_fn: F,
) -> (
    LaunchProcessStatus,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    CleanupOutcome,
)
where
    C: ChildOps + Send + 'static,
    F: FnOnce(C) -> Result<(), std::io::Error>,
{
    perform_cleanup_with_limit(
        child,
        pid,
        original_error,
        is_timeout_stop,
        stdout_path,
        stderr_path,
        DEFAULT_MAX_OUTPUT_BYTES,
        spawn_reaper_fn,
    )
}

#[allow(clippy::too_many_arguments)]
fn perform_cleanup_with_limit<C, F>(
    mut child: C,
    pid: u32,
    original_error: &str,
    is_timeout_stop: bool,
    stdout_path: &str,
    stderr_path: &str,
    max_output_bytes: usize,
    spawn_reaper_fn: F,
) -> (
    LaunchProcessStatus,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    CleanupOutcome,
)
where
    C: ChildOps + Send + 'static,
    F: FnOnce(C) -> Result<(), std::io::Error>,
{
    let (outcome, exit_res, operation_error) = match child.kill() {
        Ok(()) => match child.wait() {
            Ok(status) => (CleanupOutcome::KillSucceeded, Ok(status), None),
            Err(wait_error) => {
                let wait_error_text = wait_error.to_string();
                match spawn_reaper_fn(child) {
                    Ok(()) => (
                        CleanupOutcome::WaitFailedReaperStarted,
                        Err(wait_error),
                        Some(format!(
                            "Waiting for the terminated process failed: {wait_error_text}"
                        )),
                    ),
                    Err(spawn_error) => {
                        eprintln!(
                            "Failed to spawn background reaper during cleanup of PID {}: {}",
                            pid, spawn_error
                        );
                        (
                            CleanupOutcome::WaitFailedReaperStartFailed,
                            Err(wait_error),
                            Some(format!(
                                "Waiting for the terminated process failed: {wait_error_text}. Starting the background reaper also failed: {spawn_error}"
                            )),
                        )
                    }
                }
            }
        },
        Err(kill_error) => {
            let kill_error_text = kill_error.to_string();
            match child.try_wait() {
                Ok(Some(status)) => (
                    CleanupOutcome::KillFailedChildExited,
                    Ok(status),
                    Some(format!("Terminating the process failed: {kill_error_text}")),
                ),
                Ok(None) => match spawn_reaper_fn(child) {
                    Ok(()) => (
                        CleanupOutcome::KillFailedChildRunning {
                            reaper_started: true,
                        },
                        Err(kill_error),
                        Some(format!("Terminating the process failed: {kill_error_text}")),
                    ),
                    Err(spawn_error) => {
                        eprintln!(
                            "Failed to spawn background reaper during cleanup of PID {}: {}",
                            pid, spawn_error
                        );
                        (
                            CleanupOutcome::KillFailedChildRunning {
                                reaper_started: false,
                            },
                            Err(kill_error),
                            Some(format!(
                                "Terminating the process failed: {kill_error_text}. Starting the background reaper also failed: {spawn_error}"
                            )),
                        )
                    }
                },
                Err(status_error) => {
                    let status_error_text = status_error.to_string();
                    match spawn_reaper_fn(child) {
                        Ok(()) => (
                            CleanupOutcome::KillFailedStatusUnknown {
                                reaper_started: true,
                            },
                            Err(status_error),
                            Some(format!(
                                "Terminating the process failed: {kill_error_text}. Checking its status also failed: {status_error_text}"
                            )),
                        ),
                        Err(spawn_error) => {
                            eprintln!(
                                "Failed to spawn background reaper during cleanup of PID {}: {}",
                                pid, spawn_error
                            );
                            (
                                CleanupOutcome::KillFailedStatusUnknown {
                                    reaper_started: false,
                                },
                                Err(status_error),
                                Some(format!(
                                    "Terminating the process failed: {kill_error_text}. Checking its status also failed: {status_error_text}. Starting the background reaper also failed: {spawn_error}"
                                )),
                            )
                        }
                    }
                }
            }
        }
    };

    let status = match outcome {
        CleanupOutcome::KillFailedChildExited => LaunchProcessStatus::Completed,
        CleanupOutcome::KillSucceeded if is_timeout_stop => LaunchProcessStatus::TimedOutStopped,
        CleanupOutcome::KillSucceeded => LaunchProcessStatus::WaitFailed,
        _ if is_timeout_stop => LaunchProcessStatus::StopFailed,
        _ => LaunchProcessStatus::WaitFailed,
    };

    let operation_error = operation_error
        .map(|error| format!(" {error}."))
        .unwrap_or_default();
    let err_msg = match outcome {
        CleanupOutcome::KillSucceeded => {
            format!(
                "{}{} Process successfully terminated and reaped.",
                original_error, operation_error
            )
        }
        CleanupOutcome::KillFailedChildExited => {
            format!(
                "{}{} The child process has exited and was successfully reaped.",
                original_error, operation_error
            )
        }
        CleanupOutcome::KillFailedChildRunning {
            reaper_started: true,
        } => {
            format!(
                "{}{} The child process is still running. A background reaper started; the process may still be running and may remain unreaped if the reaper fails.",
                original_error, operation_error
            )
        }
        CleanupOutcome::KillFailedChildRunning {
            reaper_started: false,
        } => {
            format!(
                "{}{} The child process is still running. The background reaper failed to start; the process may still be running and may remain unreaped.",
                original_error, operation_error
            )
        }
        CleanupOutcome::KillFailedStatusUnknown {
            reaper_started: true,
        } => {
            format!(
                "{}{} A background reaper started. The process status is unknown; it may still be running and may remain unreaped if the reaper fails.",
                original_error, operation_error
            )
        }
        CleanupOutcome::KillFailedStatusUnknown {
            reaper_started: false,
        } => {
            format!(
                "{}{} The background reaper failed to start. The process status is unknown; it may still be running and may remain unreaped.",
                original_error, operation_error
            )
        }
        CleanupOutcome::WaitFailedReaperStarted => {
            format!(
                "{}{} A background reaper started. The process is terminated but may remain unreaped if the reaper fails.",
                original_error, operation_error
            )
        }
        CleanupOutcome::WaitFailedReaperStartFailed => {
            format!(
                "{}{} The process is terminated but may remain unreaped.",
                original_error, operation_error
            )
        }
    };

    if matches!(
        status,
        LaunchProcessStatus::TimedOutStopped | LaunchProcessStatus::Completed
    ) {
        let final_output = read_final_output(stdout_path, stderr_path, max_output_bytes);
        let error = match (outcome, final_output.error) {
            (CleanupOutcome::KillFailedChildExited, Some(read_error)) => {
                Some(format!("{err_msg} {read_error}"))
            }
            (CleanupOutcome::KillFailedChildExited, None) => Some(err_msg),
            (_, read_error) => read_error,
        };
        (
            status,
            error,
            exit_res.ok().and_then(|s| s.code()),
            final_output.stdout,
            final_output.stderr,
            outcome,
        )
    } else {
        (status, Some(err_msg), None, None, None, outcome)
    }
}

#[allow(clippy::too_many_arguments)]
fn cleanup_child(
    child: std::process::Child,
    pid: u32,
    original_error: &str,
    is_timeout_stop: bool,
    stdout_path: &str,
    stderr_path: &str,
    max_output_bytes: usize,
    background: BackgroundContext<'_>,
) -> (
    LaunchProcessStatus,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
) {
    let tx = background.tx.clone();
    let start_time = background.start_time;
    let request_id = background.request_id;
    let (status, err, exit_code, stdout, stderr, _outcome) = perform_cleanup_with_limit(
        child,
        pid,
        original_error,
        is_timeout_stop,
        stdout_path,
        stderr_path,
        max_output_bytes,
        move |child| {
            spawn_background_reaper(
                std::sync::Arc::new(std::sync::Mutex::new(Some(child))),
                pid,
                tx,
                start_time,
                request_id,
                BackgroundReaperOptions {
                    thread_name: format!("mcp-reaper-cleanup-{pid}"),
                    context: "Cleanup reaper failed",
                    output_completion: None,
                },
            )
        },
    );
    #[cfg(test)]
    if matches!(
        _outcome,
        CleanupOutcome::KillSucceeded | CleanupOutcome::KillFailedChildExited
    ) {
        test_hooks::notify_completion(pid);
    }
    (status, err, exit_code, stdout, stderr)
}

impl McpServer {
    pub async fn launch_process_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req: LaunchProcessRequest =
            match rmcp::serde_json::from_value(rmcp::serde_json::Value::Object(params.0)) {
                Ok(req) => req,
                Err(error) => {
                    return Ok(argument_error_result(missing_argument_message(
                        &error,
                        &["process_name", "environment", "detached"],
                    )));
                }
            };
        let timeout_ms = req.timeout_ms;
        let id = self.start_request(RequestData::LaunchProcess {
            command_line: command_line_for_display(&req),
            working_directory: req.working_directory.clone(),
            detached: req.detached,
            timeout_ms,
            timeout_action: req.timeout_action,
        });

        if let Err(err_msg) = validate_request(&req) {
            self.update_request(
                id,
                RequestUpdate::Rejected {
                    error: err_msg.clone(),
                },
            );
            return Ok(argument_error_result(err_msg));
        }

        let result = self.execute_launch_process_for_request(req, id).await;
        let update = RequestUpdate::LaunchProcessResponded {
            status: result.status,
            error: result.error.clone(),
            pid: result.pid,
            exit_code: result.exit_code,
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
            stdout_file: result.stdout_file.clone(),
            stderr_file: result.stderr_file.clone(),
        };

        let is_error = launch_status_is_error(result.status);
        let summary = if is_error {
            launch_process_failure_summary(&result, timeout_ms)
        } else {
            launch_process_summary(&result)
        };
        self.finish_structured_request(id, summary, &result, is_error, update)
    }

    #[cfg(test)]
    pub async fn execute_launch_process(&self, req: LaunchProcessRequest) -> LaunchProcessResult {
        self.execute_launch_process_for_request(req, RequestId(0))
            .await
    }

    async fn execute_launch_process_for_request(
        &self,
        req: LaunchProcessRequest,
        request_id: RequestId,
    ) -> LaunchProcessResult {
        let tx = self.tx.clone();
        let start_time = self.start_time;
        let join_handle = tokio::task::spawn_blocking(move || {
            execute_launch_process_blocking(req, tx, start_time, request_id)
        });
        match join_handle.await {
            Ok(res) => res,
            Err(e) => LaunchProcessResult {
                status: LaunchProcessStatus::WaitFailed,
                error: Some(format!("Spawn blocking task failed: {}", e)),
                pid: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                stdout_file: None,
                stderr_file: None,
            },
        }
    }
}

pub(crate) fn launch_process_summary(result: &LaunchProcessResult) -> String {
    match result.status {
        LaunchProcessStatus::Completed => match (result.pid, result.exit_code) {
            (Some(pid), Some(exit_code)) => {
                format!("Process {pid} completed with exit code {exit_code}.")
            }
            (Some(pid), None) => format!("Process {pid} completed."),
            (None, _) => "Process completed.".to_string(),
        },
        LaunchProcessStatus::Detached => result.pid.map_or_else(
            || "Process started and was detached.".to_string(),
            |pid| format!("Process {pid} started and was detached."),
        ),
        LaunchProcessStatus::DetachedWithStopTimeout => result.pid.map_or_else(
            || "Process started detached with a stop timeout.".to_string(),
            |pid| format!("Process {pid} started detached with a stop timeout."),
        ),
        LaunchProcessStatus::TimedOutDetached => result.pid.map_or_else(
            || "Process timed out and was detached.".to_string(),
            |pid| format!("Process {pid} timed out and was detached."),
        ),
        LaunchProcessStatus::TimedOutStopped => result.pid.map_or_else(
            || "Process timed out and was stopped.".to_string(),
            |pid| format!("Process {pid} timed out and was stopped."),
        ),
        LaunchProcessStatus::SetupFailed => "Process setup failed.".to_string(),
        LaunchProcessStatus::LaunchProcessFailed => "Process launch failed.".to_string(),
        LaunchProcessStatus::WaitFailed => result.pid.map_or_else(
            || "Waiting for the process failed.".to_string(),
            |pid| format!("Waiting for process {pid} failed."),
        ),
        LaunchProcessStatus::StopFailed => result.pid.map_or_else(
            || {
                "Stopping the process failed; successful termination could not be confirmed."
                    .to_string()
            },
            |pid| {
                format!(
                    "Stopping process {pid} failed; successful termination could not be confirmed."
                )
            },
        ),
    }
}

/// Statuses where the launch call itself failed to complete as requested; these are
/// surfaced to the model as `isError` tool results instead of successes.
fn launch_status_is_error(status: LaunchProcessStatus) -> bool {
    matches!(
        status,
        LaunchProcessStatus::TimedOutDetached
            | LaunchProcessStatus::TimedOutStopped
            | LaunchProcessStatus::SetupFailed
            | LaunchProcessStatus::LaunchProcessFailed
            | LaunchProcessStatus::WaitFailed
            | LaunchProcessStatus::StopFailed
    )
}

/// Text summary for failed launches: the concise status line plus the timeout that was
/// configured, the underlying error, and the output-file locations so the model can
/// inspect partial output and decide how to retry.
pub(crate) fn launch_process_failure_summary(
    result: &LaunchProcessResult,
    timeout_ms: Option<u64>,
) -> String {
    let mut text = launch_process_summary(result);
    if let Some(ms) = timeout_ms {
        text.push_str(&format!(" The process was allowed {ms} ms to exit."));
    }
    match result.status {
        LaunchProcessStatus::TimedOutDetached => {
            text.push_str(
                " The process may still be running; retry with a larger timeout_ms if it must \
                 complete before this call returns.",
            );
        }
        LaunchProcessStatus::TimedOutStopped => {
            text.push_str(
                " The process was terminated at the timeout; descendant processes may still be \
                 running.",
            );
        }
        LaunchProcessStatus::StopFailed => {
            text.push_str(" The process may still be running.");
        }
        _ => {}
    }
    if let Some(error) = &result.error {
        text.push(' ');
        text.push_str(error);
        if !text.ends_with('.') {
            text.push('.');
        }
    }
    if let (Some(stdout_file), Some(stderr_file)) = (&result.stdout_file, &result.stderr_file) {
        text.push_str(&format!(
            " Output was written to {stdout_file} (stdout) and {stderr_file} (stderr)."
        ));
    }
    text
}

fn execute_launch_process_blocking(
    req: LaunchProcessRequest,
    tx: Sender<UiEvent>,
    start_time: Instant,
    request_id: RequestId,
) -> LaunchProcessResult {
    let max_output_bytes = req
        .max_output_bytes
        .map(|value| usize::try_from(value).expect("validated max_output_bytes must fit usize"))
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
    let (stdout_file, stderr_file, stdout_path, stderr_path) = match generate_output_files() {
        Ok(files) => files,
        Err(e) => {
            return LaunchProcessResult {
                status: LaunchProcessStatus::SetupFailed,
                error: Some(format!("Failed to create output files: {}", e)),
                pid: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                stdout_file: None,
                stderr_file: None,
            };
        }
    };

    let working_dir = match req.working_directory {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::temp_dir(),
    };

    let mut cmd = std::process::Command::new(&req.process_name);
    cmd.current_dir(working_dir);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(stdout_file);
    cmd.stderr(stderr_file);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    if !req.environment.inherit {
        cmd.env_clear();
    }
    for (k, v) in &req.environment.variables {
        if let Some(val) = v {
            cmd.env(k, val);
        } else {
            cmd.env_remove(k);
        }
    }

    if let Some(arguments) = req
        .arguments
        .as_ref()
        .filter(|arguments| !arguments.is_empty())
    {
        cmd.args(arguments);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return LaunchProcessResult {
                status: LaunchProcessStatus::LaunchProcessFailed,
                error: Some(format!("Failed to launch process: {}", e)),
                pid: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                stdout_file: Some(stdout_path.clone()),
                stderr_file: Some(stderr_path.clone()),
            };
        }
    };

    let pid = child.id();
    let mut output_monitor = start_output_progress_monitor(
        stdout_path.clone(),
        stderr_path.clone(),
        max_output_bytes,
        pid,
        tx.clone(),
        start_time,
        request_id,
    );
    let child_arc = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    match (req.detached, req.timeout_ms, req.timeout_action) {
        (true, None, None) => {
            let output_completion = Some(output_monitor.completion_handle());
            let reaper_spawn = spawn_background_reaper(
                child_arc.clone(),
                pid,
                tx.clone(),
                start_time,
                request_id,
                BackgroundReaperOptions {
                    thread_name: format!("mcp-reaper-{pid}"),
                    context: "Detached reaper failed",
                    output_completion,
                },
            );
            if reaper_spawn.is_ok() {
                output_monitor.disarm();
            }
            match reaper_spawn {
                Ok(_) => LaunchProcessResult {
                    status: LaunchProcessStatus::Detached,
                    error: None,
                    pid: Some(pid),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                },
                Err(e) => {
                    let child_opt = child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                    let (status, error_msg, _, _, _) = if let Some(child) = child_opt {
                        let original_error =
                            format!("Failed to spawn background reaper thread: {}", e);
                        cleanup_child(
                            child,
                            pid,
                            &original_error,
                            false,
                            &stdout_path,
                            &stderr_path,
                            max_output_bytes,
                            BackgroundContext {
                                tx: &tx,
                                start_time,
                                request_id,
                            },
                        )
                    } else {
                        (
                            LaunchProcessStatus::WaitFailed,
                            Some(format!(
                                "Failed to spawn background reaper thread: {}. Process could not be accessed.",
                                e
                            )),
                            None,
                            None,
                            None,
                        )
                    };
                    LaunchProcessResult {
                        status,
                        error: error_msg,
                        pid: Some(pid),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        stdout_file: Some(stdout_path),
                        stderr_file: Some(stderr_path),
                    }
                }
            }
        }

        (true, Some(timeout_ms), Some(TimeoutAction::Stop)) => {
            let child_arc_clone = child_arc.clone();
            let monitor_stdout = stdout_path.clone();
            let monitor_stderr = stderr_path.clone();
            let tx_clone = tx.clone();
            let monitor_output = Some(output_monitor.completion_handle());
            let monitor_spawn = std::thread::Builder::new()
                .name(format!("mcp-monitor-{}", pid))
                .spawn(move || {
                    let start = std::time::Instant::now();
                    let timeout_duration = std::time::Duration::from_millis(timeout_ms);
                    let mut outcome = MonitorOutcome::TimedOut;

                    while start.elapsed() < timeout_duration {
                        let mut lock = child_arc_clone.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(ref mut child) = *lock {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    outcome = MonitorOutcome::Exited(status);
                                    break;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    outcome = MonitorOutcome::WaitFailed(e);
                                    break;
                                }
                            }
                        }
                        drop(lock);
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }

                    let child_opt = child_arc_clone
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(child) = child_opt {
                        match outcome {
                            MonitorOutcome::Exited(_status) => {
                                #[cfg(test)]
                                test_hooks::notify_completion(pid);
                            }
                            MonitorOutcome::TimedOut => {
                                let (status, error, ..) = cleanup_child(
                                    child,
                                    pid,
                                    "Process timed out",
                                    true,
                                    &monitor_stdout,
                                    &monitor_stderr,
                                    max_output_bytes,
                                    BackgroundContext {
                                        tx: &tx_clone,
                                        start_time,
                                        request_id,
                                    },
                                );
                                if !matches!(
                                    status,
                                    LaunchProcessStatus::TimedOutStopped
                                        | LaunchProcessStatus::Completed
                                ) {
                                    let error = error.unwrap_or_else(|| {
                                        "Detached timeout cleanup failed without further details"
                                            .to_string()
                                    });
                                    report_background_error(
                                        &tx_clone, start_time, request_id, pid, error,
                                    );
                                }
                            }
                            MonitorOutcome::WaitFailed(ref e) => {
                                let original_error = format!(
                                    "Detached monitor failed to check process status: {}",
                                    e
                                );
                                let (_, cleanup_error, ..) = cleanup_child(
                                    child,
                                    pid,
                                    &original_error,
                                    false,
                                    &monitor_stdout,
                                    &monitor_stderr,
                                    max_output_bytes,
                                    BackgroundContext {
                                        tx: &tx_clone,
                                        start_time,
                                        request_id,
                                    },
                                );
                                let error = cleanup_error.unwrap_or(original_error);
                                report_background_error(
                                    &tx_clone, start_time, request_id, pid, error,
                                );
                            }
                        }
                    }
                    if let Some(completion) = monitor_output {
                        completion.finish();
                    }
                });

            if monitor_spawn.is_ok() {
                output_monitor.disarm();
            }
            match monitor_spawn {
                Ok(_) => LaunchProcessResult {
                    status: LaunchProcessStatus::DetachedWithStopTimeout,
                    error: None,
                    pid: Some(pid),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                },
                Err(e) => {
                    let child_opt = child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                    let (status, error_msg, _, _, _) = if let Some(child) = child_opt {
                        let original_error =
                            format!("Failed to spawn background monitor thread: {}", e);
                        cleanup_child(
                            child,
                            pid,
                            &original_error,
                            false,
                            &stdout_path,
                            &stderr_path,
                            max_output_bytes,
                            BackgroundContext {
                                tx: &tx,
                                start_time,
                                request_id,
                            },
                        )
                    } else {
                        (
                            LaunchProcessStatus::WaitFailed,
                            Some(format!(
                                "Failed to spawn background monitor thread: {}. Process could not be accessed.",
                                e
                            )),
                            None,
                            None,
                            None,
                        )
                    };
                    LaunchProcessResult {
                        status,
                        error: error_msg,
                        pid: Some(pid),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        stdout_file: Some(stdout_path),
                        stderr_file: Some(stderr_path),
                    }
                }
            }
        }

        (false, Some(timeout_ms), Some(TimeoutAction::Detach)) => {
            let start = std::time::Instant::now();
            let timeout_duration = std::time::Duration::from_millis(timeout_ms);
            let mut exited = false;
            let mut exit_status = None;

            while start.elapsed() < timeout_duration {
                let mut lock = child_arc.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref mut child) = *lock {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            exited = true;
                            exit_status = Some(status);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            drop(lock);
                            let original_error = format!("Failed to check process status: {}", e);
                            let child_opt =
                                child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let (status, err_msg, exit_code, stdout, stderr) =
                                if let Some(child) = child_opt {
                                    cleanup_child(
                                        child,
                                        pid,
                                        &original_error,
                                        false,
                                        &stdout_path,
                                        &stderr_path,
                                        max_output_bytes,
                                        BackgroundContext {
                                            tx: &tx,
                                            start_time,
                                            request_id,
                                        },
                                    )
                                } else {
                                    (
                                        LaunchProcessStatus::WaitFailed,
                                        Some(format!(
                                            "{}. Process could not be accessed.",
                                            original_error
                                        )),
                                        None,
                                        None,
                                        None,
                                    )
                                };
                            return LaunchProcessResult {
                                status,
                                error: err_msg,
                                pid: Some(pid),
                                exit_code,
                                stdout,
                                stderr,
                                stdout_file: Some(stdout_path),
                                stderr_file: Some(stderr_path),
                            };
                        }
                    }
                }
                drop(lock);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            if exited {
                let final_output = read_final_output(&stdout_path, &stderr_path, max_output_bytes);

                LaunchProcessResult {
                    status: LaunchProcessStatus::Completed,
                    error: final_output.error,
                    pid: Some(pid),
                    exit_code: exit_status.and_then(|s| s.code()),
                    stdout: final_output.stdout,
                    stderr: final_output.stderr,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                }
            } else {
                let output_completion = Some(output_monitor.completion_handle());
                let reaper_spawn = spawn_background_reaper(
                    child_arc.clone(),
                    pid,
                    tx.clone(),
                    start_time,
                    request_id,
                    BackgroundReaperOptions {
                        thread_name: format!("mcp-reaper-{pid}"),
                        context: "Timeout-detach reaper failed",
                        output_completion,
                    },
                );
                if reaper_spawn.is_ok() {
                    output_monitor.disarm();
                }
                match reaper_spawn {
                    Ok(_) => LaunchProcessResult {
                        status: LaunchProcessStatus::TimedOutDetached,
                        error: None,
                        pid: Some(pid),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        stdout_file: Some(stdout_path),
                        stderr_file: Some(stderr_path),
                    },
                    Err(e) => {
                        let child_opt = child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                        let (status, err_msg, exit_code, stdout, stderr) = if let Some(child) =
                            child_opt
                        {
                            let original_error =
                                format!("Failed to spawn background reaper thread: {}", e);
                            cleanup_child(
                                child,
                                pid,
                                &original_error,
                                false,
                                &stdout_path,
                                &stderr_path,
                                max_output_bytes,
                                BackgroundContext {
                                    tx: &tx,
                                    start_time,
                                    request_id,
                                },
                            )
                        } else {
                            (
                                LaunchProcessStatus::WaitFailed,
                                Some(format!(
                                    "Failed to spawn background reaper thread: {}. Process could not be accessed.",
                                    e
                                )),
                                None,
                                None,
                                None,
                            )
                        };
                        LaunchProcessResult {
                            status,
                            error: err_msg,
                            pid: Some(pid),
                            exit_code,
                            stdout,
                            stderr,
                            stdout_file: Some(stdout_path),
                            stderr_file: Some(stderr_path),
                        }
                    }
                }
            }
        }

        (false, Some(timeout_ms), Some(TimeoutAction::Stop)) => {
            let start = std::time::Instant::now();
            let timeout_duration = std::time::Duration::from_millis(timeout_ms);
            let mut exited = false;
            let mut exit_status = None;

            while start.elapsed() < timeout_duration {
                let mut lock = child_arc.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref mut child) = *lock {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            exited = true;
                            exit_status = Some(status);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            drop(lock);
                            let original_error = format!("Failed to check process status: {}", e);
                            let child_opt =
                                child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let (status, err_msg, exit_code, stdout, stderr) =
                                if let Some(child) = child_opt {
                                    cleanup_child(
                                        child,
                                        pid,
                                        &original_error,
                                        false,
                                        &stdout_path,
                                        &stderr_path,
                                        max_output_bytes,
                                        BackgroundContext {
                                            tx: &tx,
                                            start_time,
                                            request_id,
                                        },
                                    )
                                } else {
                                    (
                                        LaunchProcessStatus::WaitFailed,
                                        Some(format!(
                                            "{}. Process could not be accessed.",
                                            original_error
                                        )),
                                        None,
                                        None,
                                        None,
                                    )
                                };
                            return LaunchProcessResult {
                                status,
                                error: err_msg,
                                pid: Some(pid),
                                exit_code,
                                stdout,
                                stderr,
                                stdout_file: Some(stdout_path),
                                stderr_file: Some(stderr_path),
                            };
                        }
                    }
                }
                drop(lock);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            if exited {
                let final_output = read_final_output(&stdout_path, &stderr_path, max_output_bytes);

                LaunchProcessResult {
                    status: LaunchProcessStatus::Completed,
                    error: final_output.error,
                    pid: Some(pid),
                    exit_code: exit_status.and_then(|s| s.code()),
                    stdout: final_output.stdout,
                    stderr: final_output.stderr,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                }
            } else {
                let child_opt = child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
                let (status, err_msg, exit_code, stdout, stderr) = if let Some(child) = child_opt {
                    let original_error = "Process timed out".to_string();
                    cleanup_child(
                        child,
                        pid,
                        &original_error,
                        true,
                        &stdout_path,
                        &stderr_path,
                        max_output_bytes,
                        BackgroundContext {
                            tx: &tx,
                            start_time,
                            request_id,
                        },
                    )
                } else {
                    (
                        LaunchProcessStatus::StopFailed,
                        Some(
                            "Process timed out and could not be accessed to terminate it."
                                .to_string(),
                        ),
                        None,
                        None,
                        None,
                    )
                };

                LaunchProcessResult {
                    status,
                    error: err_msg,
                    pid: Some(pid),
                    exit_code,
                    stdout,
                    stderr,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                }
            }
        }

        (false, None, None) => {
            let child_opt = child_arc.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut child) = child_opt {
                let wait_res = child.wait();
                match wait_res {
                    Ok(status) => {
                        let final_output =
                            read_final_output(&stdout_path, &stderr_path, max_output_bytes);

                        LaunchProcessResult {
                            status: LaunchProcessStatus::Completed,
                            error: final_output.error,
                            pid: Some(pid),
                            exit_code: status.code(),
                            stdout: final_output.stdout,
                            stderr: final_output.stderr,
                            stdout_file: Some(stdout_path),
                            stderr_file: Some(stderr_path),
                        }
                    }
                    Err(e) => {
                        let original_error = format!("Failed to wait for process: {}", e);
                        let (status, err_msg, exit_code, stdout, stderr) = cleanup_child(
                            child,
                            pid,
                            &original_error,
                            false,
                            &stdout_path,
                            &stderr_path,
                            max_output_bytes,
                            BackgroundContext {
                                tx: &tx,
                                start_time,
                                request_id,
                            },
                        );
                        LaunchProcessResult {
                            status,
                            error: err_msg,
                            pid: Some(pid),
                            exit_code,
                            stdout,
                            stderr,
                            stdout_file: Some(stdout_path),
                            stderr_file: Some(stderr_path),
                        }
                    }
                }
            } else {
                LaunchProcessResult {
                    status: LaunchProcessStatus::WaitFailed,
                    error: Some("Process could not be accessed to wait for it.".to_string()),
                    pid: Some(pid),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    stdout_file: Some(stdout_path),
                    stderr_file: Some(stderr_path),
                }
            }
        }
        _ => LaunchProcessResult {
            status: LaunchProcessStatus::SetupFailed,
            error: Some("Invalid request parameters combination".to_string()),
            pid: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            stdout_file: Some(stdout_path),
            stderr_file: Some(stderr_path),
        },
    }
}
