use crate::mcp::{
    LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer, TimeoutAction,
    UiEventKind,
};

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
        if req.arguments.contains('\0') {
            return Err("arguments cannot contain null characters".to_string());
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        for arg in &req.arguments {
            if arg.contains('\0') {
                return Err("arguments cannot contain null characters".to_string());
            }
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

impl McpServer {
    pub async fn launch_process_impl(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<LaunchProcessRequest>,
    ) -> Result<rmcp::handler::server::wrapper::Json<LaunchProcessResult>, rmcp::ErrorData> {
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

        Ok(rmcp::handler::server::wrapper::Json(result))
    }

    pub async fn execute_launch_process(&self, req: LaunchProcessRequest) -> LaunchProcessResult {
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
            cmd.raw_arg(&req.arguments);
        }
        #[cfg(not(target_os = "windows"))]
        {
            cmd.args(&req.arguments);
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
                let child_arc_clone = child_arc.clone();
                let reaper_spawn = std::thread::Builder::new()
                    .name(format!("mcp-reaper-{}", pid))
                    .spawn(move || {
                        let child_opt = child_arc_clone.lock().unwrap().take();
                        if let Some(mut child) = child_opt {
                            let _ = child.wait();
                            #[cfg(test)]
                            test_hooks::notify_completion(pid);
                        }
                    });

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
                        let child_opt = child_arc.lock().unwrap().take();
                        let error_msg = if let Some(mut child) = child_opt {
                            let kill_res = child.kill();
                            let wait_res = child.wait();
                            if kill_res.is_err() || wait_res.is_err() {
                                format!(
                                    "Failed to spawn background reaper thread: {}. Attempted to terminate the child, but termination/waiting failed; the process may still be running.",
                                    e
                                )
                            } else {
                                format!(
                                    "Failed to spawn background reaper thread: {}. Process was successfully terminated.",
                                    e
                                )
                            }
                        } else {
                            format!(
                                "Failed to spawn background reaper thread: {}. Process could not be accessed.",
                                e
                            )
                        };
                        LaunchProcessResult {
                            status: LaunchProcessStatus::WaitFailed,
                            error: Some(error_msg),
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
                let monitor_spawn = std::thread::Builder::new()
                    .name(format!("mcp-monitor-{}", pid))
                    .spawn(move || {
                        let start = std::time::Instant::now();
                        let timeout_duration = std::time::Duration::from_millis(timeout_ms);
                        let mut exited = false;

                        while start.elapsed() < timeout_duration {
                            let mut lock = child_arc_clone.lock().unwrap();
                            if let Some(ref mut child) = *lock {
                                match child.try_wait() {
                                    Ok(Some(_status)) => {
                                        exited = true;
                                        break;
                                    }
                                    Ok(None) => {}
                                    Err(_) => {
                                        break;
                                    }
                                }
                            }
                            drop(lock);
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }

                        let child_opt = child_arc_clone.lock().unwrap().take();
                        if let Some(mut child) = child_opt {
                            if !exited {
                                let _ = child.kill();
                            }
                            let _ = child.wait();

                            #[cfg(test)]
                            test_hooks::notify_completion(pid);
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
                        let child_opt = child_arc.lock().unwrap().take();
                        let error_msg = if let Some(mut child) = child_opt {
                            let kill_res = child.kill();
                            let wait_res = child.wait();
                            if kill_res.is_err() || wait_res.is_err() {
                                format!(
                                    "Failed to spawn background monitor thread: {}. Attempted to terminate the child, but termination/waiting failed; the process may still be running.",
                                    e
                                )
                            } else {
                                format!(
                                    "Failed to spawn background monitor thread: {}. Process was successfully terminated.",
                                    e
                                )
                            }
                        } else {
                            format!(
                                "Failed to spawn background monitor thread: {}. Process could not be accessed.",
                                e
                            )
                        };
                        LaunchProcessResult {
                            status: LaunchProcessStatus::WaitFailed,
                            error: Some(error_msg),
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
                    let mut lock = child_arc.lock().unwrap();
                    if let Some(ref mut child) = *lock {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                exited = true;
                                exit_status = Some(status);
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                return LaunchProcessResult {
                                    status: LaunchProcessStatus::WaitFailed,
                                    error: Some(format!("Failed to check process status: {}", e)),
                                    pid: Some(pid),
                                    exit_code: None,
                                    stdout: None,
                                    stderr: None,
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
                    let mut read_error = None;
                    let stdout_val = match read_and_truncate_file(&stdout_path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            read_error = Some(format!("Failed to read stdout file: {}", e));
                            None
                        }
                    };
                    let stderr_val = match read_and_truncate_file(&stderr_path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            if read_error.is_none() {
                                read_error = Some(format!("Failed to read stderr file: {}", e));
                            } else {
                                read_error =
                                    Some(format!("Failed to read stdout and stderr: {}", e));
                            }
                            None
                        }
                    };

                    LaunchProcessResult {
                        status: LaunchProcessStatus::Completed,
                        error: read_error,
                        pid: Some(pid),
                        exit_code: exit_status.and_then(|s| s.code()),
                        stdout: stdout_val,
                        stderr: stderr_val,
                        stdout_file: Some(stdout_path),
                        stderr_file: Some(stderr_path),
                    }
                } else {
                    let child_arc_clone = child_arc.clone();
                    let reaper_spawn = std::thread::Builder::new()
                        .name(format!("mcp-reaper-{}", pid))
                        .spawn(move || {
                            let child_opt = child_arc_clone.lock().unwrap().take();
                            if let Some(mut child) = child_opt {
                                let _ = child.wait();
                                #[cfg(test)]
                                test_hooks::notify_completion(pid);
                            }
                        });

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
                            let child_opt = child_arc.lock().unwrap().take();
                            let error_msg = if let Some(mut child) = child_opt {
                                let kill_res = child.kill();
                                let wait_res = child.wait();
                                if kill_res.is_err() || wait_res.is_err() {
                                    format!(
                                        "Failed to spawn background reaper thread: {}. Attempted to terminate the child, but termination/waiting failed; the process may still be running.",
                                        e
                                    )
                                } else {
                                    format!(
                                        "Failed to spawn background reaper thread: {}. Process was successfully terminated.",
                                        e
                                    )
                                }
                            } else {
                                format!(
                                    "Failed to spawn background reaper thread: {}. Process could not be accessed.",
                                    e
                                )
                            };
                            LaunchProcessResult {
                                status: LaunchProcessStatus::WaitFailed,
                                error: Some(error_msg),
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
            }

            (false, Some(timeout_ms), Some(TimeoutAction::Stop)) => {
                let start = std::time::Instant::now();
                let timeout_duration = std::time::Duration::from_millis(timeout_ms);
                let mut exited = false;
                let mut exit_status = None;

                while start.elapsed() < timeout_duration {
                    let mut lock = child_arc.lock().unwrap();
                    if let Some(ref mut child) = *lock {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                exited = true;
                                exit_status = Some(status);
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                return LaunchProcessResult {
                                    status: LaunchProcessStatus::WaitFailed,
                                    error: Some(format!("Failed to check process status: {}", e)),
                                    pid: Some(pid),
                                    exit_code: None,
                                    stdout: None,
                                    stderr: None,
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
                    let mut read_error = None;
                    let stdout_val = match read_and_truncate_file(&stdout_path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            read_error = Some(format!("Failed to read stdout: {}", e));
                            None
                        }
                    };
                    let stderr_val = match read_and_truncate_file(&stderr_path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            if read_error.is_none() {
                                read_error = Some(format!("Failed to read stderr: {}", e));
                            } else {
                                read_error =
                                    Some(format!("Failed to read stdout and stderr: {}", e));
                            }
                            None
                        }
                    };

                    LaunchProcessResult {
                        status: LaunchProcessStatus::Completed,
                        error: read_error,
                        pid: Some(pid),
                        exit_code: exit_status.and_then(|s| s.code()),
                        stdout: stdout_val,
                        stderr: stderr_val,
                        stdout_file: Some(stdout_path),
                        stderr_file: Some(stderr_path),
                    }
                } else {
                    let child_opt = child_arc.lock().unwrap().take();
                    if let Some(mut child) = child_opt {
                        let kill_res = child.kill();
                        let wait_res = child.wait();

                        if kill_res.is_err() || wait_res.is_err() {
                            let error_msg = match (kill_res, wait_res) {
                                (Err(e), _) => format!("Failed to terminate child: {}", e),
                                (_, Err(e)) => {
                                    format!("Failed to wait for terminated child: {}", e)
                                }
                                _ => unreachable!(),
                            };
                            LaunchProcessResult {
                                status: LaunchProcessStatus::StopFailed,
                                error: Some(format!("{}, process may still be running", error_msg)),
                                pid: Some(pid),
                                exit_code: None,
                                stdout: None,
                                stderr: None,
                                stdout_file: Some(stdout_path),
                                stderr_file: Some(stderr_path),
                            }
                        } else {
                            let mut read_error = None;
                            let stdout_val = match read_and_truncate_file(&stdout_path) {
                                Ok(s) => Some(s),
                                Err(e) => {
                                    read_error = Some(format!("Failed to read stdout: {}", e));
                                    None
                                }
                            };
                            let stderr_val = match read_and_truncate_file(&stderr_path) {
                                Ok(s) => Some(s),
                                Err(e) => {
                                    if read_error.is_none() {
                                        read_error = Some(format!("Failed to read stderr: {}", e));
                                    } else {
                                        read_error = Some(format!(
                                            "Failed to read stdout and stderr: {}",
                                            e
                                        ));
                                    }
                                    None
                                }
                            };

                            LaunchProcessResult {
                                status: LaunchProcessStatus::TimedOutStopped,
                                error: read_error,
                                pid: Some(pid),
                                exit_code: wait_res.ok().and_then(|s| s.code()),
                                stdout: stdout_val,
                                stderr: stderr_val,
                                stdout_file: Some(stdout_path),
                                stderr_file: Some(stderr_path),
                            }
                        }
                    } else {
                        LaunchProcessResult {
                            status: LaunchProcessStatus::StopFailed,
                            error: Some(
                                "Process could not be accessed to terminate it.".to_string(),
                            ),
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

            (false, None, None) => {
                let child_opt = child_arc.lock().unwrap().take();
                if let Some(mut child) = child_opt {
                    let wait_res = child.wait();
                    match wait_res {
                        Ok(status) => {
                            let mut read_error = None;
                            let stdout_val = match read_and_truncate_file(&stdout_path) {
                                Ok(s) => Some(s),
                                Err(e) => {
                                    read_error = Some(format!("Failed to read stdout: {}", e));
                                    None
                                }
                            };
                            let stderr_val = match read_and_truncate_file(&stderr_path) {
                                Ok(s) => Some(s),
                                Err(e) => {
                                    if read_error.is_none() {
                                        read_error = Some(format!("Failed to read stderr: {}", e));
                                    } else {
                                        read_error = Some(format!(
                                            "Failed to read stdout and stderr: {}",
                                            e
                                        ));
                                    }
                                    None
                                }
                            };

                            LaunchProcessResult {
                                status: LaunchProcessStatus::Completed,
                                error: read_error,
                                pid: Some(pid),
                                exit_code: status.code(),
                                stdout: stdout_val,
                                stderr: stderr_val,
                                stdout_file: Some(stdout_path),
                                stderr_file: Some(stderr_path),
                            }
                        }
                        Err(e) => LaunchProcessResult {
                            status: LaunchProcessStatus::WaitFailed,
                            error: Some(format!("Failed to wait for process: {}", e)),
                            pid: Some(pid),
                            exit_code: None,
                            stdout: None,
                            stderr: None,
                            stdout_file: Some(stdout_path),
                            stderr_file: Some(stderr_path),
                        },
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
}
