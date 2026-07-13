use crate::mcp::{
    LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer, TimeoutAction,
    UiEvent, UiEventKind,
};
use std::sync::mpsc::Sender;
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

pub fn read_and_truncate_file(path: &str) -> Result<String, std::io::Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();

    if len == 0 {
        return Ok(String::new());
    }

    let limit = 1024usize;
    let (to_read, truncated) = if len > limit as u64 {
        (limit, true)
    } else {
        (len as usize, false)
    };

    let mut buffer = vec![0u8; to_read];
    if truncated {
        file.seek(SeekFrom::End(-(limit as i64)))?;
    }
    file.read_exact(&mut buffer)?;

    let decoded = String::from_utf8_lossy(&buffer).into_owned();
    if truncated {
        Ok(format!("[... beginning truncated ...]\n{}", decoded))
    } else {
        Ok(decoded)
    }
}

struct FinalOutput {
    error: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

fn read_final_output(stdout_path: &str, stderr_path: &str) -> FinalOutput {
    let mut errors = Vec::new();
    let stdout = match read_and_truncate_file(stdout_path) {
        Ok(output) => Some(output),
        Err(error) => {
            errors.push(format!("Failed to read stdout: {error}"));
            None
        }
    };
    let stderr = match read_and_truncate_file(stderr_path) {
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

    #[cfg(target_os = "windows")]
    {
        if req
            .arguments
            .as_ref()
            .is_some_and(|arguments| arguments.contains('\0'))
        {
            return Err("arguments cannot contain null characters".to_string());
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if req
            .arguments
            .as_ref()
            .is_some_and(|arguments| arguments.iter().any(|argument| argument.contains('\0')))
        {
            return Err("arguments cannot contain null characters".to_string());
        }
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

    Ok(())
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
    pid: u32,
    error: String,
) {
    eprintln!("Background process handling failed for PID {pid}: {error}");
    let _ = tx.send(UiEvent {
        elapsed: start_time.elapsed(),
        kind: UiEventKind::LaunchProcessBackgroundError { pid, error },
    });
}

fn handle_background_wait_result(
    wait_result: std::io::Result<std::process::ExitStatus>,
    pid: u32,
    tx: &Sender<UiEvent>,
    start_time: Instant,
    context: &str,
) {
    handle_background_wait_result_with_notifier(wait_result, pid, tx, start_time, context, |pid| {
        #[cfg(test)]
        test_hooks::notify_completion(pid);
        #[cfg(not(test))]
        let _ = pid;
    });
}

pub(crate) fn handle_background_wait_result_with_notifier<F>(
    wait_result: std::io::Result<std::process::ExitStatus>,
    pid: u32,
    tx: &Sender<UiEvent>,
    start_time: Instant,
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
            pid,
            format!(
                "{context}: {error}. Successful reaping could not be confirmed; the process may remain running or unreaped"
            ),
        ),
    }
}

fn spawn_background_reaper(
    child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>,
    pid: u32,
    tx: Sender<UiEvent>,
    start_time: Instant,
    thread_name: String,
    context: &'static str,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let child = child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(mut child) = child {
                let wait_result = child.wait();
                handle_background_wait_result(wait_result, pid, &tx, start_time, context);
            }
        })
        .map(|_| ())
}

#[derive(Clone, Copy)]
struct BackgroundContext<'a> {
    tx: &'a Sender<UiEvent>,
    start_time: Instant,
}

pub(crate) fn perform_cleanup<C, F>(
    mut child: C,
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
        let final_output = read_final_output(stdout_path, stderr_path);
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

fn cleanup_child(
    child: std::process::Child,
    pid: u32,
    original_error: &str,
    is_timeout_stop: bool,
    stdout_path: &str,
    stderr_path: &str,
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
    let (status, err, exit_code, stdout, stderr, _outcome) = perform_cleanup(
        child,
        pid,
        original_error,
        is_timeout_stop,
        stdout_path,
        stderr_path,
        move |child| {
            spawn_background_reaper(
                std::sync::Arc::new(std::sync::Mutex::new(Some(child))),
                pid,
                tx,
                start_time,
                format!("mcp-reaper-cleanup-{pid}"),
                "Cleanup reaper failed",
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
        params: rmcp::handler::server::wrapper::Parameters<LaunchProcessRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let req = params.0;

        if let Err(err_msg) = validate_request(&req) {
            self.send_event(UiEventKind::LaunchProcessRejected {
                error: err_msg.clone(),
            });
            return Err(rmcp::ErrorData::invalid_params(err_msg, None));
        }

        self.send_event(UiEventKind::LaunchProcessRequested {
            process_name: req.process_name.clone(),
        });

        let result = self.execute_launch_process(req).await;

        self.send_event(UiEventKind::LaunchProcessResponded {
            status: result.status,
            pid: result.pid,
        });

        let summary = launch_process_summary(&result);
        Self::structured_success(summary, &result)
    }

    pub async fn execute_launch_process(&self, req: LaunchProcessRequest) -> LaunchProcessResult {
        let tx = self.tx.clone();
        let start_time = self.start_time;
        let join_handle = tokio::task::spawn_blocking(move || {
            execute_launch_process_blocking(req, tx, start_time)
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

fn execute_launch_process_blocking(
    req: LaunchProcessRequest,
    tx: Sender<UiEvent>,
    start_time: Instant,
) -> LaunchProcessResult {
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

    #[cfg(target_os = "windows")]
    {
        if let Some(arguments) = req
            .arguments
            .as_ref()
            .filter(|arguments| !arguments.is_empty())
        {
            cmd.raw_arg(arguments);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(arguments) = req
            .arguments
            .as_ref()
            .filter(|arguments| !arguments.is_empty())
        {
            cmd.args(arguments);
        }
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
    let child_arc = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));

    match (req.detached, req.timeout_ms, req.timeout_action) {
        (true, None, None) => {
            let reaper_spawn = spawn_background_reaper(
                child_arc.clone(),
                pid,
                tx.clone(),
                start_time,
                format!("mcp-reaper-{pid}"),
                "Detached reaper failed",
            );

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
                            BackgroundContext {
                                tx: &tx,
                                start_time,
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
                                    BackgroundContext {
                                        tx: &tx_clone,
                                        start_time,
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
                                    report_background_error(&tx_clone, start_time, pid, error);
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
                                    BackgroundContext {
                                        tx: &tx_clone,
                                        start_time,
                                    },
                                );
                                let error = cleanup_error.unwrap_or(original_error);
                                report_background_error(&tx_clone, start_time, pid, error);
                            }
                        }
                    }
                });

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
                            BackgroundContext {
                                tx: &tx,
                                start_time,
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
                                        BackgroundContext {
                                            tx: &tx,
                                            start_time,
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
                let final_output = read_final_output(&stdout_path, &stderr_path);

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
                let reaper_spawn = spawn_background_reaper(
                    child_arc.clone(),
                    pid,
                    tx.clone(),
                    start_time,
                    format!("mcp-reaper-{pid}"),
                    "Timeout-detach reaper failed",
                );

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
                                BackgroundContext {
                                    tx: &tx,
                                    start_time,
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
                                        BackgroundContext {
                                            tx: &tx,
                                            start_time,
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
                let final_output = read_final_output(&stdout_path, &stderr_path);

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
                        BackgroundContext {
                            tx: &tx,
                            start_time,
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
                        let final_output = read_final_output(&stdout_path, &stderr_path);

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
                            BackgroundContext {
                                tx: &tx,
                                start_time,
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
