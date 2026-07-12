use crate::mcp::launch_process::{read_and_truncate_file, validate_request};
use crate::mcp::{
    EnvironmentConfig, LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer,
    TimeoutAction, UiEventKind, run_mcp_server_loop, test_hooks,
};
use std::time::{Duration, Instant};

fn make_helper_request() -> LaunchProcessRequest {
    #[cfg(target_os = "windows")]
    let process_name = "cmd.exe".to_string();
    #[cfg(target_os = "windows")]
    let arguments = "/c \"echo hello\"".to_string();

    #[cfg(not(target_os = "windows"))]
    let process_name = "sh".to_string();
    #[cfg(not(target_os = "windows"))]
    let arguments = vec!["-c".to_string(), "echo hello".to_string()];

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
    #[cfg(target_os = "windows")]
    {
        req.arguments = "/c \"echo stdout: hello & echo stderr: hello 1>&2\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec![
            "-c".to_string(),
            "echo stdout: hello && echo stderr: hello >&2".to_string(),
        ];
    }

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
    #[cfg(target_os = "windows")]
    {
        req.arguments = "/c \"exit 42\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec!["-c".to_string(), "exit 42".to_string()];
    }
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
    #[cfg(target_os = "windows")]
    {
        req.arguments = "/c \"cd\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec!["-c".to_string(), "pwd".to_string()];
    }
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
    let test_uuid = std::process::id();
    let explicit_dir = std::env::temp_dir().join(format!("rmcp_test_wd_{}", test_uuid));
    std::fs::create_dir_all(&explicit_dir).unwrap();

    let mut req = make_helper_request();
    req.working_directory = Some(explicit_dir.to_string_lossy().into_owned());
    #[cfg(target_os = "windows")]
    {
        req.arguments = "/c \"cd\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec!["-c".to_string(), "pwd".to_string()];
    }
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

    // 3. Nonexistent working directory returns launch_failed
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

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let mut req = make_helper_request();
    req.environment.inherit = true;
    req.environment
        .variables
        .insert(var_override.to_string(), Some("overridden_val".to_string()));
    req.environment
        .variables
        .insert(var_remove.to_string(), None);

    #[cfg(target_os = "windows")]
    {
        req.arguments = format!(
            "/c \"echo {}=%{}%&echo {}=%{}%&echo {}=%{}%&echo {}=%{}%\"",
            var_inherit,
            var_inherit,
            var_override,
            var_override,
            var_remove,
            var_remove,
            var_unrelated,
            var_unrelated
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec![
            "-c".to_string(),
            format!(
                "echo {}=${}; echo {}=${}; echo {}=${}; echo {}=${}",
                var_inherit,
                var_inherit,
                var_override,
                var_override,
                var_remove,
                var_remove,
                var_unrelated,
                var_unrelated
            ),
        ];
    }

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    let stdout = res.stdout.unwrap();

    assert!(stdout.contains(&format!("{}={}", var_inherit, "inherited_val")));
    assert!(stdout.contains(&format!("{}={}", var_override, "overridden_val")));
    #[cfg(target_os = "windows")]
    assert!(stdout.contains(&format!("{}=%{}%", var_remove, var_remove)));
    #[cfg(not(target_os = "windows"))]
    assert!(stdout.contains(&format!("{}=", var_remove)));

    assert!(stdout.contains(&format!("{}={}", var_unrelated, "unrelated_val")));

    // 2. Inherit = false
    let mut req = make_helper_request();
    req.environment.inherit = false;
    let custom_var = "RMCP_TEST_CUSTOM";
    req.environment
        .variables
        .insert(custom_var.to_string(), Some("custom_val".to_string()));
    req.environment
        .variables
        .insert("HARmless_NULL".to_string(), None);

    #[cfg(target_os = "windows")]
    {
        req.arguments = format!(
            "/c \"echo {}=%{}%&echo {}=%{}%\"",
            custom_var, custom_var, var_unrelated, var_unrelated
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = vec![
            "-c".to_string(),
            format!(
                "echo {}=${}; echo {}=${}",
                custom_var, custom_var, var_unrelated, var_unrelated
            ),
        ];
    }

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    let stdout = res.stdout.unwrap();
    assert!(stdout.contains(&format!("{}={}", custom_var, "custom_val")));
    #[cfg(target_os = "windows")]
    assert!(stdout.contains(&format!("{}=%{}%", var_unrelated, var_unrelated)));
    #[cfg(not(target_os = "windows"))]
    assert!(stdout.contains(&format!("{}=", var_unrelated)));

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
    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = "-NoProfile -Command \"if ([Console]::In.ReadLine() -eq $null) { Write-Output 'STDIN_EOF' } else { Write-Output 'STDIN_NOT_EOF' }\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            "read line; if [ $? -ne 0 ]; then echo STDIN_EOF; else echo STDIN_NOT_EOF; fi"
                .to_string(),
        ];
    }

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
    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = "-NoProfile -Command \"Write-Output ('A'*2000 + 'END_OF_STDOUT'); [Console]::Error.WriteLine('B'*2000 + 'END_OF_STDERR')\"".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            "python3 -c \"print('A'*2000 + 'END_OF_STDOUT'); import sys; print('B'*2000 + 'END_OF_STDERR', file=sys.stderr)\"".to_string()
        ];
    }

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

    let marker_path =
        std::env::temp_dir().join(format!("rmcp_detach_marker_{}", std::process::id()));
    let mut req = make_helper_request();
    req.detached = true;

    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = format!(
            "-NoProfile -Command \"Start-Sleep -Milliseconds 200; 'done' | Out-File -FilePath '{}'\"",
            marker_path.to_string_lossy()
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            format!("sleep 0.2; echo done > '{}'", marker_path.to_string_lossy()),
        ];
    }

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(
        elapsed < Duration::from_millis(150),
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
        .recv_timeout(Duration::from_millis(1000))
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

    let marker_path =
        std::env::temp_dir().join(format!("rmcp_timeout_detach_marker_{}", std::process::id()));
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(50);
    req.timeout_action = Some(TimeoutAction::Detach);

    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = format!(
            "-NoProfile -Command \"Start-Sleep -Milliseconds 300; 'done' | Out-File -FilePath '{}'\"",
            marker_path.to_string_lossy()
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            format!("sleep 0.3; echo done > '{}'", marker_path.to_string_lossy()),
        ];
    }

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(matches!(res.status, LaunchProcessStatus::TimedOutDetached));
    assert!(
        elapsed < Duration::from_millis(250),
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
        .recv_timeout(Duration::from_millis(1000))
        .unwrap();
    assert_eq!(completed_pid, pid);
    assert!(marker_path.exists());
    let _ = std::fs::remove_file(&marker_path);

    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(500);
    req.timeout_action = Some(TimeoutAction::Detach);
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

    let marker_path =
        std::env::temp_dir().join(format!("rmcp_timeout_stop_marker_{}", std::process::id()));
    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(500);
    req.timeout_action = Some(TimeoutAction::Stop);

    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = format!(
            "-NoProfile -Command \"Write-Output 'partial_out'; [Console]::Error.WriteLine('partial_err'); Start-Sleep -Milliseconds 2000; 'done' | Out-File -FilePath '{}'\"",
            marker_path.to_string_lossy()
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            format!(
                "echo 'partial_out'; echo 'partial_err' >&2; sleep 2.0; echo done > '{}'",
                marker_path.to_string_lossy()
            ),
        ];
    }

    let res = rt.block_on(async { server.execute_launch_process(req).await });

    assert!(matches!(res.status, LaunchProcessStatus::TimedOutStopped));
    assert!(res.pid.is_some());

    std::thread::sleep(Duration::from_millis(100));
    assert!(!marker_path.exists());

    assert_eq!(res.stdout.as_deref().unwrap().trim(), "partial_out");
    assert_eq!(res.stderr.as_deref().unwrap().trim(), "partial_err");

    let mut req = make_helper_request();
    req.detached = false;
    req.timeout_ms = Some(2000);
    req.timeout_action = Some(TimeoutAction::Stop);
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

    let marker_path =
        std::env::temp_dir().join(format!("rmcp_det_stop_marker_{}", std::process::id()));
    let mut req = make_helper_request();
    req.detached = true;
    req.timeout_ms = Some(50);
    req.timeout_action = Some(TimeoutAction::Stop);

    #[cfg(target_os = "windows")]
    {
        req.process_name = "powershell.exe".to_string();
        req.arguments = format!(
            "-NoProfile -Command \"Start-Sleep -Milliseconds 500; 'done' | Out-File -FilePath '{}'\"",
            marker_path.to_string_lossy()
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.process_name = "sh".to_string();
        req.arguments = vec![
            "-c".to_string(),
            format!("sleep 0.5; echo done > '{}'", marker_path.to_string_lossy()),
        ];
    }

    let start_time = Instant::now();
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    let elapsed = start_time.elapsed();

    assert!(
        elapsed < Duration::from_millis(150),
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
        .recv_timeout(Duration::from_millis(1000))
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

        let variables: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();

        #[cfg(target_os = "windows")]
        let base_arguments_val = rmcp::serde_json::json!(
            "/c \"echo stdout: integration_test & echo stderr: integration_test 1>&2\""
        );
        #[cfg(not(target_os = "windows"))]
        let base_arguments_val = rmcp::serde_json::json!(vec![
            "-c".to_string(),
            "echo stdout: integration_test && echo stderr: integration_test >&2".to_string()
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
