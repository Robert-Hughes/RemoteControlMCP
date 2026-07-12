use crate::mcp::launch_process::{classify_cleanup, read_and_truncate_file, validate_request};
use crate::mcp::{
    EnvironmentConfig, LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer,
    TimeoutAction, UiEventKind, run_mcp_server_loop, test_hooks,
};
use std::time::{Duration, Instant};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "windows")]
fn escape_windows_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.contains([' ', '\t', '\n', '\x0b', '\"']) {
        return arg.to_string();
    }
    let mut escaped = String::new();
    escaped.push('\"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '\"' => {
                escaped.push_str(&"\\".repeat(backslashes * 2 + 1));
                escaped.push('\"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    escaped.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                escaped.push(c);
            }
        }
    }
    if backslashes > 0 {
        escaped.push_str(&"\\".repeat(backslashes * 2));
    }
    escaped.push('\"');
    escaped
}

#[cfg(target_os = "windows")]
fn escape_windows_args(args: &[&str]) -> String {
    args.iter()
        .map(|arg| escape_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn generate_temp_test_path(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let thread_id = format!("{:?}", std::thread::current().id());
    let thread_id_clean: String = thread_id.chars().filter(|c| c.is_alphanumeric()).collect();
    let name = format!("rmcp_{}_{}_{}_{}", prefix, pid, thread_id_clean, id);
    let path = std::env::temp_dir().join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn make_helper_request() -> LaunchProcessRequest {
    let process_name = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    #[cfg(target_os = "windows")]
    let arguments = escape_windows_args(&["launch_process_test_helper", "--ignored"]);

    #[cfg(not(target_os = "windows"))]
    let arguments = vec![
        "launch_process_test_helper".to_string(),
        "--ignored".to_string(),
    ];

    LaunchProcessRequest {
        working_directory: None,
        process_name,
        arguments,
        environment: EnvironmentConfig {
            inherit: true,
            variables: std::collections::HashMap::new(),
        },
        detached: false,
        timeout_ms: None,
        timeout_action: None,
    }
}

struct HelperOutputs {
    stdout: std::fs::File,
    stderr: std::fs::File,
}

impl HelperOutputs {
    fn new() -> Self {
        #[cfg(target_os = "windows")]
        {
            use std::io::{Seek, SeekFrom};
            use std::os::windows::io::{AsRawHandle, FromRawHandle};
            let stdout_raw = std::io::stdout().as_raw_handle();
            let mut stdout = unsafe { std::fs::File::from_raw_handle(stdout_raw) };
            let _ = stdout.set_len(0);
            let _ = stdout.seek(SeekFrom::Start(0));

            let stderr_raw = std::io::stderr().as_raw_handle();
            let mut stderr = unsafe { std::fs::File::from_raw_handle(stderr_raw) };
            let _ = stderr.set_len(0);
            let _ = stderr.seek(SeekFrom::Start(0));
            Self { stdout, stderr }
        }
        #[cfg(not(target_os = "windows"))]
        {
            use std::io::{Seek, SeekFrom};
            use std::os::unix::io::{AsRawFd, FromRawFd};
            let stdout_raw = std::io::stdout().as_raw_fd();
            let mut stdout = unsafe { std::fs::File::from_raw_fd(stdout_raw) };
            let _ = stdout.set_len(0);
            let _ = stdout.seek(SeekFrom::Start(0));

            let stderr_raw = std::io::stderr().as_raw_fd();
            let mut stderr = unsafe { std::fs::File::from_raw_fd(stderr_raw) };
            let _ = stderr.set_len(0);
            let _ = stderr.seek(SeekFrom::Start(0));
            Self { stdout, stderr }
        }
    }

    fn write_stdout(&mut self, s: &str) {
        use std::io::Write;
        let _ = self.stdout.write_all(s.as_bytes());
        let _ = self.stdout.flush();
    }

    fn write_stdout_bytes(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = self.stdout.write_all(bytes);
        let _ = self.stdout.flush();
    }

    fn write_stderr(&mut self, s: &str) {
        use std::io::Write;
        let _ = self.stderr.write_all(s.as_bytes());
        let _ = self.stderr.flush();
    }
}

#[test]
#[ignore]
fn launch_process_test_helper() {
    if let Ok(action) = std::env::var("RMCP_TEST_HELPER_ACTION") {
        let mut outputs = HelperOutputs::new();
        match action.as_str() {
            "stdout_stderr" => {
                let stdout_val = std::env::var("RMCP_TEST_HELPER_STDOUT").unwrap_or_default();
                let stderr_val = std::env::var("RMCP_TEST_HELPER_STDERR").unwrap_or_default();
                outputs.write_stdout(&stdout_val);
                outputs.write_stderr(&stderr_val);
                std::process::exit(0);
            }
            "exit_code" => {
                let code: i32 = std::env::var("RMCP_TEST_HELPER_CODE")
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or(0);
                std::process::exit(code);
            }
            "pwd" => {
                if let Ok(cwd) = std::env::current_dir() {
                    outputs.write_stdout(&cwd.to_string_lossy());
                }
                std::process::exit(0);
            }
            "env" => {
                let name = std::env::var("RMCP_TEST_HELPER_ENV_NAME").unwrap_or_default();
                if let Ok(val) = std::env::var(&name) {
                    outputs.write_stdout(&val);
                }
                std::process::exit(0);
            }
            "stdin_eof" => {
                use std::io::Read;
                let mut buffer = String::new();
                let stdin_res = std::io::stdin().read_to_string(&mut buffer);
                if stdin_res.is_ok() {
                    outputs.write_stdout("STDIN_EOF");
                } else {
                    outputs.write_stdout("STDIN_ERROR");
                }
                std::process::exit(0);
            }
            "sleep" => {
                let ms: u64 = std::env::var("RMCP_TEST_HELPER_SLEEP_MS")
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or(0);
                if let Ok(val) = std::env::var("RMCP_TEST_HELPER_PARTIAL_STDOUT") {
                    outputs.write_stdout(&format!("{}\n", val));
                }
                if let Ok(val) = std::env::var("RMCP_TEST_HELPER_PARTIAL_STDERR") {
                    outputs.write_stderr(&format!("{}\n", val));
                }
                std::thread::sleep(std::time::Duration::from_millis(ms));
                if let Ok(marker) = std::env::var("RMCP_TEST_HELPER_MARKER") {
                    let _ = std::fs::write(&marker, "done");
                }
                std::process::exit(0);
            }
            "large_output" => {
                let count: usize = std::env::var("RMCP_TEST_HELPER_COUNT")
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or(2000);
                let stdout_char = std::env::var("RMCP_TEST_HELPER_STDOUT_CHAR")
                    .unwrap_or_else(|_| "A".to_string());
                let stderr_char = std::env::var("RMCP_TEST_HELPER_STDERR_CHAR")
                    .unwrap_or_else(|_| "B".to_string());
                let stdout_tail = std::env::var("RMCP_TEST_HELPER_STDOUT_TAIL").unwrap_or_default();
                let stderr_tail = std::env::var("RMCP_TEST_HELPER_STDERR_TAIL").unwrap_or_default();

                outputs.write_stdout(&format!("{}{}", stdout_char.repeat(count), stdout_tail));
                outputs.write_stderr(&format!("{}{}", stderr_char.repeat(count), stderr_tail));
                std::process::exit(0);
            }
            "invalid_utf8" => {
                outputs.write_stdout_bytes(&[0xff, 0xff, 0xff, 0xff]);
                std::process::exit(0);
            }
            "echo_args" => {
                let args: Vec<String> = std::env::args().collect();
                if let Some(pos) = args.iter().position(|x| x == "launch_process_test_helper") {
                    let helper_args = &args[pos + 1..];
                    let filtered: Vec<&String> = helper_args
                        .iter()
                        .filter(|x| *x != "--ignored" && *x != "--nocapture")
                        .collect();
                    let formatted = filtered
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("|");
                    outputs.write_stdout(&formatted);
                } else {
                    outputs.write_stdout("no_helper_arg");
                }
                std::process::exit(0);
            }
            _ => {
                std::process::exit(0);
            }
        }
    } else {
        std::process::exit(0);
    }
}

#[test]
fn ping_returns_pong() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();
    let server = McpServer::new(tx, start_time);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let result = rt.block_on(async { server.ping().await });
    assert_eq!(result, "pong");
}

#[test]
fn ping_emits_request_and_response_events() {
    let (tx, rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();
    let server = McpServer::new(tx, start_time);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _result = rt.block_on(async { server.ping().await });

    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert_eq!(
        events,
        vec![UiEventKind::PingRequested, UiEventKind::PingResponded]
    );
}

#[test]
fn ping_metadata_is_read_only_and_idempotent() {
    let attr = McpServer::ping_tool_attr();
    assert_eq!(attr.name, "ping");
    assert!(attr.description.is_some());

    let ann = attr
        .annotations
        .as_ref()
        .expect("annotations should be present");
    assert_eq!(ann.read_only_hint, Some(true));
    assert_eq!(ann.destructive_hint, Some(false));
    assert_eq!(ann.idempotent_hint, Some(true));
    assert_eq!(ann.open_world_hint, Some(false));

    assert_eq!(
        attr.input_schema.get("type"),
        Some(&rmcp::serde_json::Value::String("object".to_string()))
    );
    if let Some(properties) = attr.input_schema.get("properties") {
        assert!(properties.as_object().is_none_or(|p| p.is_empty()));
    }
}

#[test]
fn ping_works_over_mcp_duplex_transport() {
    let (tx, rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();

    let (server_transport, client_transport) = tokio::io::duplex(4096);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tx_clone = tx.clone();
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx_clone, start_time, server_transport).await;
        });

        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("Failed to serve client");

        // 1. Tool discovery through tools/list
        let tools = client.list_all_tools().await.expect("Failed to list tools");
        assert_eq!(tools.len(), 2);
        let tool = tools
            .iter()
            .find(|t| t.name == "ping")
            .expect("ping tool not found");
        assert_eq!(tool.name, "ping");
        assert!(tool.description.is_some());

        // 2. Tool metadata returned over MCP matches annotations
        let ann = tool
            .annotations
            .as_ref()
            .expect("annotations should be present");
        assert_eq!(ann.read_only_hint, Some(true));
        assert_eq!(ann.destructive_hint, Some(false));
        assert_eq!(ann.idempotent_hint, Some(true));
        assert_eq!(ann.open_world_hint, Some(false));

        // 3. Tool execution through tools/call
        let call_params = rmcp::model::CallToolRequestParams::new("ping");
        let call_result = client
            .call_tool(call_params)
            .await
            .expect("Failed to call tool");

        // 4. MCP text-result decoding
        assert_eq!(call_result.content.len(), 1);
        match &call_result.content[0] {
            rmcp::model::ContentBlock::Text(tc) => {
                assert_eq!(tc.text, "pong");
            }
            _ => panic!("Expected Text content block"),
        }

        // 5. Graceful client/server shutdown
        client.close().await.expect("Failed to close client");
        server_task.await.expect("Server task panicked");
    });

    // 6. UI lifecycle and tool events
    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    let expected_subsequence = &[
        UiEventKind::ServerStarting,
        UiEventKind::WaitingForClient,
        UiEventKind::ClientConnected,
        UiEventKind::PingRequested,
        UiEventKind::PingResponded,
    ];
    let mut event_iter = events.iter();
    for expected in expected_subsequence {
        loop {
            match event_iter.next() {
                Some(e) if e == expected => break,
                Some(_) => continue,
                None => panic!(
                    "Expected event sequence {:?} not found in actual events {:?}",
                    expected_subsequence, events
                ),
            }
        }
    }

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEventKind::ServerError { .. })),
        "unexpected server error during shutdown: {events:?}"
    );

    assert_eq!(
        events.last(),
        Some(&UiEventKind::ServerStopped),
        "expected graceful shutdown to end with ServerStopped; events: {events:?}"
    );
}

#[test]
fn test_validation() {
    let base_req = make_helper_request();

    // 1. Empty process name
    let mut req = base_req.clone();
    req.process_name = "".to_string();
    assert!(validate_request(&req).is_err());

    // 2. Null character in process name
    let mut req = base_req.clone();
    req.process_name = "test\0exe".to_string();
    assert!(validate_request(&req).is_err());

    // 3. Null character in working directory
    let mut req = base_req.clone();
    req.working_directory = Some("C:\\temp\0".to_string());
    assert!(validate_request(&req).is_err());

    // 4. Null character in Windows raw arguments, under cfg(windows)
    #[cfg(target_os = "windows")]
    {
        let mut req = base_req.clone();
        req.arguments = "some\0args".to_string();
        assert!(validate_request(&req).is_err());
    }

    // 5. Null character in an argument-array item, under cfg(not(windows))
    #[cfg(not(target_os = "windows"))]
    {
        let mut req = base_req.clone();
        req.arguments = vec!["arg1".to_string(), "arg\0two".to_string()];
        assert!(validate_request(&req).is_err());
    }

    // 6. Empty environment-variable name
    let mut req = base_req.clone();
    req.environment
        .variables
        .insert("".to_string(), Some("val".to_string()));
    assert!(validate_request(&req).is_err());

    // 7. Environment-variable name containing =
    let mut req = base_req.clone();
    req.environment
        .variables
        .insert("VAR=NAME".to_string(), Some("val".to_string()));
    assert!(validate_request(&req).is_err());

    // 8. Environment-variable name containing a null character
    let mut req = base_req.clone();
    req.environment
        .variables
        .insert("VAR\0NAME".to_string(), Some("val".to_string()));
    assert!(validate_request(&req).is_err());

    // 9. Environment-variable value containing a null character
    let mut req = base_req.clone();
    req.environment
        .variables
        .insert("VARNAME".to_string(), Some("val\0".to_string()));
    assert!(validate_request(&req).is_err());

    // 10. timeout_ms = 0
    let mut req = base_req.clone();
    req.timeout_ms = Some(0);
    req.timeout_action = Some(TimeoutAction::Detach);
    assert!(validate_request(&req).is_err());

    // 11. Timeout without action
    let mut req = base_req.clone();
    req.timeout_ms = Some(100);
    req.timeout_action = None;
    assert!(validate_request(&req).is_err());

    // 12. Action without timeout
    let mut req = base_req.clone();
    req.timeout_ms = None;
    req.timeout_action = Some(TimeoutAction::Detach);
    assert!(validate_request(&req).is_err());

    // 13. Detached launch with timeout action detach
    let mut req = base_req.clone();
    req.detached = true;
    req.timeout_ms = Some(100);
    req.timeout_action = Some(TimeoutAction::Detach);
    assert!(validate_request(&req).is_err());

    // Valid request validation test
    let req = base_req.clone();
    assert!(validate_request(&req).is_ok());
}

#[test]
fn test_schema_arguments() {
    let attr = McpServer::launch_process_tool_attr();
    let properties = attr
        .input_schema
        .get("properties")
        .unwrap()
        .as_object()
        .unwrap();
    let args_schema = properties.get("arguments").unwrap().as_object().unwrap();

    #[cfg(target_os = "windows")]
    {
        assert_eq!(args_schema.get("type").unwrap().as_str().unwrap(), "string");
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(args_schema.get("type").unwrap().as_str().unwrap(), "array");
    }
}

#[test]
fn test_successful_completion() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    // Exit zero
    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("stdout_stderr".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDOUT".to_string(),
        Some("stdout: hello\n".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDERR".to_string(),
        Some("stderr: hello\n".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });

    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.exit_code, Some(0));
    assert!(res.pid.is_some());

    let stdout_trimmed = res.stdout.as_deref().unwrap().trim();
    let stderr_trimmed = res.stderr.as_deref().unwrap().trim();
    assert_eq!(stdout_trimmed, "stdout: hello");
    assert_eq!(stderr_trimmed, "stderr: hello");

    let stdout_file = res.stdout_file.unwrap();
    let stderr_file = res.stderr_file.unwrap();
    assert!(std::path::Path::new(&stdout_file).exists());
    assert!(std::path::Path::new(&stderr_file).exists());

    // Verify full contents of files
    let stdout_full = std::fs::read_to_string(&stdout_file).unwrap();
    let stderr_full = std::fs::read_to_string(&stderr_file).unwrap();
    assert_eq!(stdout_full.trim(), "stdout: hello");
    assert_eq!(stderr_full.trim(), "stderr: hello");

    // Non-zero exit code
    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("exit_code".to_string()),
    );
    req.environment
        .variables
        .insert("RMCP_TEST_HELPER_CODE".to_string(), Some("42".to_string()));
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.exit_code, Some(42));
    assert!(res.pid.is_some());
}

#[test]
fn test_working_directory() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    // 1. Omitted working directory uses std::env::temp_dir()
    let mut req = make_helper_request();
    req.working_directory = None;
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("pwd".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    let temp_dir_str = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let stdout_str = res.stdout.unwrap().trim().to_lowercase();
    let parsed_cwd = std::path::Path::new(&stdout_str)
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    assert!(parsed_cwd.contains(&temp_dir_str) || temp_dir_str.contains(&parsed_cwd));

    // 2. Explicitly supplied working directory is used
    let explicit_dir = generate_temp_test_path("wd");
    std::fs::create_dir_all(&explicit_dir).unwrap();

    let mut req = make_helper_request();
    req.working_directory = Some(explicit_dir.to_string_lossy().into_owned());
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("pwd".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));

    let expected_dir = explicit_dir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let actual_dir = std::path::Path::new(&res.stdout.unwrap().trim())
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    assert_eq!(actual_dir, expected_dir);

    let _ = std::fs::remove_dir_all(&explicit_dir);

    // 3. Nonexistent working directory returns launch_process_failed
    let mut req = make_helper_request();
    req.working_directory = Some(
        std::env::temp_dir()
            .join("nonexistent_dir_123456")
            .to_string_lossy()
            .into_owned(),
    );
    assert!(validate_request(&req).is_ok());

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(
        res.status,
        LaunchProcessStatus::LaunchProcessFailed
    ));
    assert!(res.error.is_some());
}

#[test]
fn test_environment_handling() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let var_inherit = "RMCP_TEST_INHERIT";
    let var_override = "RMCP_TEST_OVERRIDE";
    let var_remove = "RMCP_TEST_REMOVE";
    let var_unrelated = "RMCP_TEST_UNRELATED";

    unsafe {
        std::env::set_var(var_inherit, "inherited_val");
        std::env::set_var(var_override, "parent_val");
        std::env::set_var(var_remove, "parent_val");
        std::env::set_var(var_unrelated, "unrelated_val");
    }

    // 1. Inherit = true
    let query_env =
        |inherit: bool, var_name: &str, override_val: Option<&str>, remove_var: Option<&str>| {
            let mut req = make_helper_request();
            req.environment.inherit = inherit;
            req.environment.variables.insert(
                "RMCP_TEST_HELPER_ACTION".to_string(),
                Some("env".to_string()),
            );
            req.environment.variables.insert(
                "RMCP_TEST_HELPER_ENV_NAME".to_string(),
                Some(var_name.to_string()),
            );
            if let Some(o_val) = override_val {
                req.environment
                    .variables
                    .insert(var_override.to_string(), Some(o_val.to_string()));
            }
            if let Some(r_var) = remove_var {
                req.environment.variables.insert(r_var.to_string(), None);
            }
            let res = rt.block_on(async { server.execute_launch_process(req).await });
            assert!(matches!(res.status, LaunchProcessStatus::Completed));
            res.stdout.unwrap()
        };

    assert_eq!(query_env(true, var_inherit, None, None), "inherited_val");
    assert_eq!(
        query_env(true, var_override, Some("overridden_val"), None),
        "overridden_val"
    );
    assert_eq!(query_env(true, var_remove, None, Some(var_remove)), "");
    assert_eq!(query_env(true, var_unrelated, None, None), "unrelated_val");

    // 2. Inherit = false
    assert_eq!(query_env(false, var_inherit, None, None), "");
    let custom_var = "RMCP_TEST_CUSTOM";
    let mut req = make_helper_request();
    req.environment.inherit = false;
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("env".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ENV_NAME".to_string(),
        Some(custom_var.to_string()),
    );
    req.environment
        .variables
        .insert(custom_var.to_string(), Some("custom_val".to_string()));
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.stdout.unwrap(), "custom_val");

    unsafe {
        std::env::remove_var(var_inherit);
        std::env::remove_var(var_override);
        std::env::remove_var(var_remove);
        std::env::remove_var(var_unrelated);
    }
}

#[test]
fn test_null_stdin() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("stdin_eof".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.stdout.as_deref().unwrap().trim(), "STDIN_EOF");
}

#[test]
fn test_output_truncation_logic() {
    let temp_dir = std::env::temp_dir().join("rmcp_test_truncation");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("test_trunc.txt");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // 1. Empty output
    std::fs::write(&file_path, "").unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert_eq!(res, "");

    // 2. Output shorter than 1024 bytes
    let short_data = "Hello World!";
    std::fs::write(&file_path, short_data).unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert_eq!(res, short_data);

    // 3. Output exactly 1024 bytes
    let exact_data = "X".repeat(1024);
    std::fs::write(&file_path, &exact_data).unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert_eq!(res, exact_data);

    // 4. Output 1025 bytes
    let data_1025 = "Y".repeat(1025);
    std::fs::write(&file_path, &data_1025).unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert!(res.starts_with("[... beginning truncated ...]\n"));
    let retained = res.strip_prefix("[... beginning truncated ...]\n").unwrap();
    assert_eq!(retained, &data_1025[1..]);

    // 5. Much larger output
    let mut large_data = "Z".repeat(5000);
    large_data.push_str("TAIL_INFO");
    std::fs::write(&file_path, &large_data).unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert!(res.starts_with("[... beginning truncated ...]\n"));
    let retained = res.strip_prefix("[... beginning truncated ...]\n").unwrap();
    assert_eq!(retained.len(), 1024);
    assert!(retained.ends_with("TAIL_INFO"));

    // 6. Lossy UTF-8 conversion for invalid byte sequences
    std::fs::write(&file_path, [0xff, 0xff, 0xff, 0xff]).unwrap();
    let res = read_and_truncate_file(&file_path_str).unwrap();
    assert_eq!(res, "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_real_helper_truncation() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("large_output".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_COUNT".to_string(),
        Some("2000".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDOUT_CHAR".to_string(),
        Some("A".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDERR_CHAR".to_string(),
        Some("B".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDOUT_TAIL".to_string(),
        Some("END_OF_STDOUT".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_STDERR_TAIL".to_string(),
        Some("END_OF_STDERR".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));

    assert!(
        res.stdout
            .as_ref()
            .unwrap()
            .starts_with("[... beginning truncated ...]\n")
    );
    assert!(
        res.stderr
            .as_ref()
            .unwrap()
            .starts_with("[... beginning truncated ...]\n")
    );

    let stdout_retained = res
        .stdout
        .as_ref()
        .unwrap()
        .strip_prefix("[... beginning truncated ...]\n")
        .unwrap();
    let stderr_retained = res
        .stderr
        .as_ref()
        .unwrap()
        .strip_prefix("[... beginning truncated ...]\n")
        .unwrap();

    assert_eq!(stdout_retained.len(), 1024);
    assert_eq!(stderr_retained.len(), 1024);
    assert!(stdout_retained.trim().ends_with("END_OF_STDOUT"));
    assert!(stderr_retained.trim().ends_with("END_OF_STDERR"));

    let stdout_file = res.stdout_file.unwrap();
    let stderr_file = res.stderr_file.unwrap();
    let stdout_full = std::fs::read_to_string(&stdout_file).unwrap();
    let stderr_full = std::fs::read_to_string(&stderr_file).unwrap();

    assert_eq!(stdout_full.trim().len(), 2013);
    assert_eq!(stderr_full.trim().len(), 2013);
    assert!(stdout_full.starts_with("AAAA"));
    assert!(stderr_full.starts_with("BBBB"));
}

#[test]
fn test_unique_output_files() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let req = make_helper_request();

    let res1 = rt.block_on(async { server.execute_launch_process(req.clone()).await });
    let res2 = rt.block_on(async { server.execute_launch_process(req.clone()).await });
    let res3 = rt.block_on(async { server.execute_launch_process(req.clone()).await });

    assert!(matches!(res1.status, LaunchProcessStatus::Completed));
    assert!(matches!(res2.status, LaunchProcessStatus::Completed));
    assert!(matches!(res3.status, LaunchProcessStatus::Completed));

    let paths = vec![
        res1.stdout_file.unwrap(),
        res1.stderr_file.unwrap(),
        res2.stdout_file.unwrap(),
        res2.stderr_file.unwrap(),
        res3.stdout_file.unwrap(),
        res3.stderr_file.unwrap(),
    ];

    let mut unique_paths = paths.clone();
    unique_paths.sort();
    unique_paths.dedup();
    assert_eq!(unique_paths.len(), paths.len());

    let expected_prefix = std::env::temp_dir()
        .join("RemoteControlMCP")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    for path in paths {
        let canon_path = std::path::Path::new(&path)
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(canon_path.contains(&expected_prefix));
    }
}

#[test]
fn test_explicit_detachment() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let (completion_tx, completion_rx) = std::sync::mpsc::channel();
    test_hooks::register_completion_sender(completion_tx);

    let marker_path = generate_temp_test_path("detach_marker");
    let mut req = make_helper_request();
    req.detached = true;
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("1500".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_MARKER".to_string(),
        Some(marker_path.to_string_lossy().into_owned()),
    );

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(
        elapsed < Duration::from_millis(750),
        "Should return promptly, elapsed: {:?}",
        elapsed
    );
    assert!(matches!(res.status, LaunchProcessStatus::Detached));
    assert!(res.pid.is_some());
    assert!(res.stdout_file.is_some());
    assert!(res.stderr_file.is_some());
    assert!(res.stdout.is_none());
    assert!(res.stderr.is_none());
    assert!(res.exit_code.is_none());

    let pid = res.pid.unwrap();

    let completed_pid = completion_rx
        .recv_timeout(Duration::from_millis(5000))
        .unwrap();
    assert_eq!(completed_pid, pid);

    assert!(marker_path.exists());
    let _ = std::fs::remove_file(&marker_path);
}

#[test]
fn test_timeout_with_detach() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let (completion_tx, completion_rx) = std::sync::mpsc::channel();
    test_hooks::register_completion_sender(completion_tx);

    let marker_path = generate_temp_test_path("timeout_detach_marker");
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(150);
    req.timeout_action = Some(TimeoutAction::Detach);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("1500".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_MARKER".to_string(),
        Some(marker_path.to_string_lossy().into_owned()),
    );

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(matches!(res.status, LaunchProcessStatus::TimedOutDetached));
    assert!(
        elapsed < Duration::from_millis(750),
        "Should return before child completion, elapsed: {:?}",
        elapsed
    );
    assert!(res.pid.is_some());
    assert!(res.stdout_file.is_some());
    assert!(res.stderr_file.is_some());
    assert!(res.stdout.is_none());
    assert!(res.stderr.is_none());
    assert!(res.exit_code.is_none());

    let pid = res.pid.unwrap();

    let completed_pid = completion_rx
        .recv_timeout(Duration::from_millis(5000))
        .unwrap();
    assert_eq!(completed_pid, pid);
    assert!(marker_path.exists());
    let _ = std::fs::remove_file(&marker_path);

    // Timeout large enough to complete naturally
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(2000);
    req.timeout_action = Some(TimeoutAction::Detach);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("100".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
}

#[test]
fn test_timeout_with_stop() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let marker_path = generate_temp_test_path("timeout_stop_marker");
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(200);
    req.timeout_action = Some(TimeoutAction::Stop);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("2000".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_PARTIAL_STDOUT".to_string(),
        Some("partial_out".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_PARTIAL_STDERR".to_string(),
        Some("partial_err".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_MARKER".to_string(),
        Some(marker_path.to_string_lossy().into_owned()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });

    assert!(matches!(res.status, LaunchProcessStatus::TimedOutStopped));
    assert!(res.pid.is_some());

    std::thread::sleep(Duration::from_millis(200));
    assert!(!marker_path.exists());

    assert_eq!(res.stdout.as_deref().unwrap().trim(), "partial_out");
    assert_eq!(res.stderr.as_deref().unwrap().trim(), "partial_err");

    // Timeout large enough to complete naturally
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(3000);
    req.timeout_action = Some(TimeoutAction::Stop);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("100".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
}

#[test]
fn test_detached_with_stop_timeout() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let (completion_tx, completion_rx) = std::sync::mpsc::channel();
    test_hooks::register_completion_sender(completion_tx);

    let marker_path = generate_temp_test_path("det_stop_marker");
    let mut req = make_helper_request();
    req.detached = true;
    req.timeout_ms = Some(200);
    req.timeout_action = Some(TimeoutAction::Stop);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("2000".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_MARKER".to_string(),
        Some(marker_path.to_string_lossy().into_owned()),
    );

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(
        elapsed < Duration::from_millis(750),
        "Should return promptly, elapsed: {:?}",
        elapsed
    );
    assert!(matches!(
        res.status,
        LaunchProcessStatus::DetachedWithStopTimeout
    ));
    assert!(res.pid.is_some());
    assert!(res.stdout_file.is_some());
    assert!(res.stderr_file.is_some());
    assert!(res.stdout.is_none());
    assert!(res.stderr.is_none());
    assert!(res.exit_code.is_none());

    let pid = res.pid.unwrap();

    let completed_pid = completion_rx
        .recv_timeout(Duration::from_millis(5000))
        .unwrap();
    assert_eq!(completed_pid, pid);

    assert!(!marker_path.exists());
}

#[test]
fn test_failure_results() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut req = make_helper_request();
    req.process_name = "nonexistent_executable_123456789.exe".to_string();
    let res = rt.block_on(async { server.execute_launch_process(req).await });

    assert!(matches!(
        res.status,
        LaunchProcessStatus::LaunchProcessFailed
    ));
    assert!(res.error.is_some());
    assert!(res.pid.is_none());
    assert!(res.exit_code.is_none());
    assert!(res.stdout_file.is_some());
    assert!(res.stderr_file.is_some());
}

#[test]
fn test_gui_events_launch_process() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let req = make_helper_request();
    let params = rmcp::handler::server::wrapper::Parameters(req);
    let res = rt.block_on(async { server.launch_process(params).await.unwrap() });
    assert!(matches!(res.0.status, LaunchProcessStatus::Completed));

    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        UiEventKind::LaunchProcessRequested { .. }
    ));
    if let UiEventKind::LaunchProcessRequested { ref process_name } = events[0] {
        assert_eq!(process_name, &make_helper_request().process_name);
    } else {
        panic!("Expected LaunchProcessRequested");
    }

    assert!(matches!(
        events[1],
        UiEventKind::LaunchProcessResponded { .. }
    ));
    if let UiEventKind::LaunchProcessResponded { status, pid } = events[1] {
        assert_eq!(status, LaunchProcessStatus::Completed);
        assert_eq!(pid, res.0.pid);
    } else {
        panic!("Expected LaunchProcessResponded");
    }

    let (tx2, rx2) = std::sync::mpsc::channel();
    let server2 = McpServer::new(tx2, Instant::now());

    let params = rmcp::handler::server::wrapper::Parameters(LaunchProcessRequest {
        working_directory: None,
        process_name: "".to_string(),
        #[cfg(target_os = "windows")]
        arguments: "".to_string(),
        #[cfg(not(target_os = "windows"))]
        arguments: vec![],
        environment: EnvironmentConfig {
            inherit: true,
            variables: std::collections::HashMap::new(),
        },
        detached: false,
        timeout_ms: None,
        timeout_action: None,
    });

    let call_res = rt.block_on(async { server2.launch_process(params).await });

    assert!(call_res.is_err());
    let events2: Vec<UiEventKind> = rx2.try_iter().map(|e| e.kind).collect();
    assert_eq!(events2.len(), 1);
    assert!(matches!(
        events2[0],
        UiEventKind::LaunchProcessRejected { .. }
    ));
}

#[test]
fn launch_process_integration_test_over_duplex() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();

    let (server_transport, client_transport) = tokio::io::duplex(8192);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tx_clone = tx.clone();
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx_clone, start_time, server_transport).await;
        });

        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("Failed to serve client");

        // 1. Tool discovery integration test
        let tools = client.list_all_tools().await.expect("Failed to list tools");
        assert_eq!(tools.len(), 2);

        let launch_tool = tools
            .iter()
            .find(|t| t.name == "launch_process")
            .expect("launch_process tool not found");
        assert_eq!(launch_tool.name, "launch_process");
        assert!(launch_tool.description.is_some());

        let ann = launch_tool
            .annotations
            .as_ref()
            .expect("annotations should be present");
        assert_eq!(ann.read_only_hint, Some(false));
        assert_eq!(ann.destructive_hint, Some(true));
        assert_eq!(ann.idempotent_hint, Some(false));
        assert_eq!(ann.open_world_hint, Some(true));

        let properties = launch_tool
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        let args_schema = properties.get("arguments").unwrap().as_object().unwrap();
        #[cfg(target_os = "windows")]
        {
            assert_eq!(args_schema.get("type").unwrap().as_str().unwrap(), "string");
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(args_schema.get("type").unwrap().as_str().unwrap(), "array");
        }

        let mut variables: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        variables.insert(
            "RMCP_TEST_HELPER_ACTION".to_string(),
            Some("stdout_stderr".to_string()),
        );
        variables.insert(
            "RMCP_TEST_HELPER_STDOUT".to_string(),
            Some("stdout: integration_test\n".to_string()),
        );
        variables.insert(
            "RMCP_TEST_HELPER_STDERR".to_string(),
            Some("stderr: integration_test\n".to_string()),
        );

        #[cfg(target_os = "windows")]
        let base_arguments_val = rmcp::serde_json::json!(escape_windows_args(&[
            "launch_process_test_helper",
            "--ignored"
        ]));
        #[cfg(not(target_os = "windows"))]
        let base_arguments_val = rmcp::serde_json::json!(vec![
            "launch_process_test_helper".to_string(),
            "--ignored".to_string()
        ]);

        let mut call_params = rmcp::model::CallToolRequestParams::new("launch_process");
        call_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": make_helper_request().process_name,
                "arguments": base_arguments_val,
                "environment": {
                    "inherit": true,
                    "variables": variables
                },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let call_result = client
            .call_tool(call_params)
            .await
            .expect("Failed to call launch_process");

        let struct_val = call_result
            .structured_content
            .expect("Expected structured content");
        let result: LaunchProcessResult = rmcp::serde_json::from_value(struct_val).unwrap();

        assert!(matches!(result.status, LaunchProcessStatus::Completed));
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.stdout.as_deref().unwrap().trim(),
            "stdout: integration_test"
        );
        assert_eq!(
            result.stderr.as_deref().unwrap().trim(),
            "stderr: integration_test"
        );

        // 3. Validation-error integration test
        let mut invalid_call_params = rmcp::model::CallToolRequestParams::new("launch_process");
        invalid_call_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": make_helper_request().process_name,
                "arguments": base_arguments_val,
                "environment": {
                    "inherit": true,
                    "variables": {}
                },
                "detached": false,
                "timeout_ms": 100
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let call_err = client.call_tool(invalid_call_params).await.unwrap_err();
        match call_err {
            rmcp::ServiceError::McpError(err_data) => {
                assert_eq!(err_data.code.0, -32602);
            }
            other => panic!("Expected McpError, got: {:?}", other),
        }

        let ping_params = rmcp::model::CallToolRequestParams::new("ping");
        let ping_result = client
            .call_tool(ping_params)
            .await
            .expect("Failed to call ping after validation error");
        assert_eq!(ping_result.content.len(), 1);

        // 4. Graceful client/server shutdown
        client.close().await.expect("Failed to close client");
        server_task.await.expect("Server task panicked");
    });

    // 5. Verify GUI event subsequence
    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    let expected_subsequence = &[
        UiEventKind::ServerStarting,
        UiEventKind::WaitingForClient,
        UiEventKind::ClientConnected,
        UiEventKind::LaunchProcessRequested {
            process_name: make_helper_request().process_name,
        },
        UiEventKind::LaunchProcessResponded {
            status: LaunchProcessStatus::Completed,
            pid: None,
        },
        UiEventKind::LaunchProcessRejected {
            error: "timeout_ms requires timeout_action".to_string(),
        },
        UiEventKind::PingRequested,
        UiEventKind::PingResponded,
        UiEventKind::ServerStopped,
    ];

    let mut event_iter = events.iter();
    for expected in expected_subsequence {
        loop {
            match (event_iter.next(), expected) {
                (
                    Some(UiEventKind::LaunchProcessRequested { .. }),
                    UiEventKind::LaunchProcessRequested { .. },
                ) => break,
                (
                    Some(UiEventKind::LaunchProcessResponded { status: s1, .. }),
                    UiEventKind::LaunchProcessResponded { status: s2, .. },
                ) if s1 == s2 => break,
                (
                    Some(UiEventKind::LaunchProcessRejected { .. }),
                    UiEventKind::LaunchProcessRejected { .. },
                ) => break,
                (Some(e), exp) if e == exp => break,
                (Some(_), _) => continue,
                (None, exp) => panic!(
                    "Expected event {:?} not found in actual events {:?}",
                    exp, events
                ),
            }
        }
    }
}

#[test]
fn test_concurrency_blocking_off_runtime() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();

    let (server_transport, client_transport) = tokio::io::duplex(8192);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tx_clone = tx.clone();
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx_clone, start_time, server_transport).await;
        });

        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("Failed to serve client");

        let client_for_launch = client.clone();
        let client_for_ping = client.clone();

        let launch_handle = tokio::spawn(async move {
            let mut variables = std::collections::HashMap::new();
            variables.insert(
                "RMCP_TEST_HELPER_ACTION".to_string(),
                Some("sleep".to_string()),
            );
            variables.insert(
                "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
                Some("1500".to_string()),
            );

            #[cfg(target_os = "windows")]
            let arguments_val = rmcp::serde_json::json!(escape_windows_args(&[
                "launch_process_test_helper",
                "--ignored"
            ]));
            #[cfg(not(target_os = "windows"))]
            let arguments_val = rmcp::serde_json::json!(vec![
                "launch_process_test_helper".to_string(),
                "--ignored".to_string()
            ]);

            let mut call_params = rmcp::model::CallToolRequestParams::new("launch_process");
            call_params.arguments = Some(
                rmcp::serde_json::json!({
                    "process_name": std::env::current_exe().unwrap().to_string_lossy(),
                    "arguments": arguments_val,
                    "environment": {
                        "inherit": true,
                        "variables": variables
                    },
                    "detached": false
                })
                .as_object()
                .unwrap()
                .clone(),
            );

            let start = Instant::now();
            let res = client_for_launch.call_tool(call_params).await;
            (res, start.elapsed())
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        let ping_start = Instant::now();
        let ping_params = rmcp::model::CallToolRequestParams::new("ping");
        let ping_result = client_for_ping
            .call_tool(ping_params)
            .await
            .expect("Failed to call ping");
        let ping_elapsed = ping_start.elapsed();

        assert_eq!(ping_result.content.len(), 1);
        match &ping_result.content[0] {
            rmcp::model::ContentBlock::Text(tc) => {
                assert_eq!(tc.text, "pong");
            }
            _ => panic!("Expected Text content block"),
        }
        assert!(
            ping_elapsed < Duration::from_millis(750),
            "Ping took too long, suggesting the runtime was blocked: {:?}",
            ping_elapsed
        );

        let (launch_res, launch_elapsed) = launch_handle.await.expect("Launch task panicked");
        let call_result = launch_res.expect("Failed to call launch_process");
        let struct_val = call_result
            .structured_content
            .expect("Expected structured content");
        let result: LaunchProcessResult = rmcp::serde_json::from_value(struct_val).unwrap();

        assert!(matches!(result.status, LaunchProcessStatus::Completed));
        assert_eq!(result.exit_code, Some(0));
        assert!(launch_elapsed >= Duration::from_millis(1500));

        client.close().await.expect("Failed to close client");
        server_task.await.expect("Server task panicked");
    });
}

#[test]
fn test_argument_boundaries() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("echo_args".to_string()),
    );

    #[cfg(target_os = "windows")]
    {
        let helper_args = vec![
            "launch_process_test_helper",
            "--ignored",
            "arg1",
            "arg 2",
            "arg\"3",
        ];
        req.arguments = escape_windows_args(&helper_args);
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec![
            "launch_process_test_helper".to_string(),
            "--ignored".to_string(),
            "arg1".to_string(),
            "arg 2".to_string(),
            "arg\"3".to_string(),
        ];
    }

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.stdout.unwrap().trim(), "arg1|arg 2|arg\"3");
}

#[test]
fn test_classify_cleanup_logic() {
    // 1. Both failed
    let (status, err, spawn_reaper) = classify_cleanup(false, false, "status failed", false);
    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert!(err.contains("status failed"));
    assert!(err.contains("Both termination and waiting failed; the process may still be running and may remain unreaped."));
    assert!(spawn_reaper);

    // 2. Kill failed, wait succeeded
    let (status, err, spawn_reaper) = classify_cleanup(false, true, "status failed", false);
    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert!(err.contains("status failed"));
    assert!(err.contains("Termination failed, but waiting succeeded; the process is not running but may have exited on its own."));
    assert!(!spawn_reaper);

    // 3. Kill succeeded, wait failed
    let (status, err, spawn_reaper) = classify_cleanup(true, false, "status failed", false);
    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert!(err.contains("status failed"));
    assert!(err.contains("Termination succeeded, but waiting failed; the process is terminated but may remain unreaped."));
    assert!(spawn_reaper);

    // 4. Both succeeded (non-timeout)
    let (status, err, spawn_reaper) = classify_cleanup(true, true, "status failed", false);
    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert!(err.contains("status failed"));
    assert!(err.contains("The process was successfully terminated and reaped; it is not running."));
    assert!(!spawn_reaper);

    // 5. Both succeeded (timeout stop)
    let (status, err, spawn_reaper) = classify_cleanup(true, true, "timed out", true);
    assert_eq!(status, LaunchProcessStatus::TimedOutStopped);
    assert!(err.contains("timed out"));
    assert!(!spawn_reaper);

    // 6. Kill failed (timeout stop)
    let (status, err, spawn_reaper) = classify_cleanup(false, true, "timed out", true);
    assert_eq!(status, LaunchProcessStatus::StopFailed);
    assert!(err.contains("timed out"));
    assert!(!spawn_reaper);
}

#[test]
fn test_invalid_utf8_lossy() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut req = make_helper_request();
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("invalid_utf8".to_string()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.stdout.unwrap(), "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}");
}
