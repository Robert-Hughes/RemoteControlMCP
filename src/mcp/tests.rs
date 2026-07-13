use crate::mcp::launch_process::{
    ChildOps, CleanupOutcome, handle_background_wait_result_with_notifier, launch_process_summary,
    perform_cleanup, read_and_truncate_file, report_background_error, validate_request,
};
use crate::mcp::ping::PingResult;
use crate::mcp::read_file::{
    install_blocking_test_hook, open_regular_file_with_metadata, read_file_summary,
    validate_read_file_request,
};
use crate::mcp::{
    EnvironmentConfig, LaunchProcessRequest, LaunchProcessResult, LaunchProcessStatus, McpServer,
    ReadFileRequest, ReadFileResult, ReadFileStatus, RequestData, RequestId, RequestUpdate,
    TimeoutAction, UiEventKind, build_mcp_runtime, run_mcp_server_loop, test_hooks,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn mcp_runtime_supports_tokio_timers() {
    let rt = build_mcp_runtime().expect("MCP runtime should build");

    rt.block_on(async {
        let result =
            tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await;

        assert!(result.is_err(), "pending future should time out");
    });
}

#[test]
fn test_background_monitor_error_event() {
    let (tx, rx) = std::sync::mpsc::channel();
    report_background_error(
        &tx,
        Instant::now(),
        RequestId(7),
        42,
        "status check failed".to_string(),
    );
    let event = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        event.kind,
        UiEventKind::RequestUpdated {
            id: RequestId(7),
            update: RequestUpdate::LaunchProcessBackgroundError { pid: 42, ref error },
        } if error == "status check failed"
    ));
}

#[test]
fn test_successful_background_wait_notifies_without_error_event() {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (completion_tx, completion_rx) = std::sync::mpsc::channel();

    handle_background_wait_result_with_notifier(
        Ok(successful_exit_status()),
        43,
        &event_tx,
        Instant::now(),
        RequestId(8),
        "Detached reaper failed",
        move |pid| completion_tx.send(pid).unwrap(),
    );

    assert_eq!(completion_rx.try_recv(), Ok(43));
    assert!(matches!(
        event_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn test_failed_background_wait_reports_error_without_success_notification() {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (completion_tx, completion_rx) = std::sync::mpsc::channel();

    handle_background_wait_result_with_notifier(
        Err(std::io::Error::other("injected wait failure")),
        44,
        &event_tx,
        Instant::now(),
        RequestId(9),
        "Timeout-detach reaper failed",
        move |pid| completion_tx.send(pid).unwrap(),
    );

    assert!(completion_rx.try_recv().is_err());
    let event = event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let UiEventKind::RequestUpdated {
        id,
        update: RequestUpdate::LaunchProcessBackgroundError { pid, error },
    } = event.kind
    else {
        panic!("expected background error event");
    };
    assert_eq!(id, RequestId(9));
    assert_eq!(pid, 44);
    assert!(error.contains("Timeout-detach reaper failed"));
    assert!(!error.contains("PID 44"));
    assert!(error.contains("injected wait failure"));
    assert!(error.contains("Successful reaping could not be confirmed"));
    assert!(error.contains("may remain running or unreaped"));
    for sensitive_input in [
        "secret argument",
        "SECRET_ENV",
        "private stdout",
        "private stderr",
    ] {
        assert!(!error.contains(sensitive_input));
    }
}

#[test]
fn environment_inherit_deserialisation_defaults_and_validation() {
    let omitted: EnvironmentConfig = rmcp::serde_json::from_value(rmcp::serde_json::json!({
        "variables": {}
    }))
    .unwrap();
    assert!(omitted.inherit);

    let explicit_true: EnvironmentConfig = rmcp::serde_json::from_value(rmcp::serde_json::json!({
        "inherit": true,
        "variables": {}
    }))
    .unwrap();
    assert!(explicit_true.inherit);

    let explicit_false: EnvironmentConfig = rmcp::serde_json::from_value(rmcp::serde_json::json!({
        "inherit": false,
        "variables": {}
    }))
    .unwrap();
    assert!(!explicit_false.inherit);

    assert!(
        rmcp::serde_json::from_value::<EnvironmentConfig>(rmcp::serde_json::json!({
            "inherit": null,
            "variables": {}
        }))
        .is_err()
    );
    assert!(
        rmcp::serde_json::from_value::<EnvironmentConfig>(rmcp::serde_json::json!({
            "inherit": true
        }))
        .is_err()
    );
    assert!(
        rmcp::serde_json::from_value::<LaunchProcessRequest>(rmcp::serde_json::json!({
            "process_name": "test",
            "detached": false
        }))
        .is_err()
    );
}

fn resolve_local_schema_ref<'a>(
    root: &'a rmcp::serde_json::Value,
    mut schema: &'a rmcp::serde_json::Value,
) -> &'a rmcp::serde_json::Value {
    while let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
        let pointer = reference
            .strip_prefix('#')
            .expect("schema reference should be local");
        schema = root
            .pointer(pointer)
            .expect("schema reference should resolve within the schema");
    }
    schema
}

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

fn write_temp_test_file(prefix: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = generate_temp_test_path(prefix);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn make_read_file_request(
    path: &std::path::Path,
    start_line: u64,
    end_line: u64,
) -> ReadFileRequest {
    ReadFileRequest {
        path: path.to_string_lossy().into_owned(),
        start_line,
        end_line,
    }
}

fn call_read_file_direct(req: ReadFileRequest) -> (rmcp::model::CallToolResult, Vec<UiEventKind>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let result = rt
        .block_on(async {
            server
                .read_file(rmcp::handler::server::wrapper::Parameters(req))
                .await
        })
        .unwrap();
    let events = rx.try_iter().map(|event| event.kind).collect();
    (result, events)
}

fn read_file_structured_result(result: &rmcp::model::CallToolResult) -> ReadFileResult {
    rmcp::serde_json::from_value(
        result
            .structured_content
            .clone()
            .expect("read_file should return structured content"),
    )
    .unwrap()
}

fn only_text_content(result: &rmcp::model::CallToolResult) -> &str {
    assert_eq!(result.content.len(), 1);
    let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected exactly one text content block");
    };
    &text.text
}

fn make_helper_request() -> LaunchProcessRequest {
    let test_executable = std::env::current_exe().unwrap();
    let debug_directory = test_executable.parent().unwrap().parent().unwrap();
    let process_name = debug_directory
        .join("examples")
        .join(format!(
            "launch_process_test_helper{}",
            std::env::consts::EXE_SUFFIX
        ))
        .to_string_lossy()
        .into_owned();

    LaunchProcessRequest {
        working_directory: None,
        process_name,
        arguments: None,
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
    let result = rt.block_on(async { server.ping_impl().await });
    assert_eq!(result, "pong");
}

#[test]
fn ping_returns_text_and_structured_content() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let result = rt.block_on(async { server.ping().await }).unwrap();
    assert_eq!(result.content.len(), 1);
    let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected ping to return one text content block");
    };
    assert_eq!(text.text, "pong");
    assert_eq!(
        result.structured_content,
        Some(rmcp::serde_json::json!({ "message": "pong" }))
    );
    assert_eq!(result.is_error, Some(false));
}

#[test]
fn ping_emits_request_and_response_events() {
    let (tx, rx) = std::sync::mpsc::channel();
    let start_time = Instant::now();
    let server = McpServer::new(tx, start_time);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _result = rt.block_on(async { server.ping().await }).unwrap();

    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert_eq!(events.len(), 2);
    let UiEventKind::RequestStarted {
        id,
        request: RequestData::Ping,
        ..
    } = events[0]
    else {
        panic!("expected ping request start");
    };
    assert_eq!(
        events[1],
        UiEventKind::RequestUpdated {
            id,
            update: RequestUpdate::PingCompleted,
        }
    );
}

#[test]
fn request_ids_are_nonzero_shared_by_clones_and_unique_under_overlap() {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let first = server.start_request(RequestData::Ping);
    let second = server.clone().start_request(RequestData::Ping);
    assert_eq!(first.get(), 1);
    assert_ne!(first, second);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let server = server.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            server.start_request(RequestData::Ping)
        }));
    }
    barrier.wait();
    let ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().get())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 8);
    assert!(!ids.contains(&0));
    assert_eq!(rx.try_iter().count(), 10);
}

#[test]
fn response_serialisation_failure_emits_internal_failure_not_completion() {
    struct FailingSerialize;

    impl serde::Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("injected serialisation failure"))
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let id = server.start_request(RequestData::Ping);
    let result = server.finish_structured_request(
        id,
        "unused".to_string(),
        &FailingSerialize,
        RequestUpdate::PingCompleted,
    );
    assert!(result.is_err());
    let events = rx.try_iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1],
        UiEventKind::RequestUpdated {
            id: update_id,
            update: RequestUpdate::InternalFailure { error },
        } if *update_id == id && error.contains("injected serialisation failure")
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        UiEventKind::RequestUpdated {
            update: RequestUpdate::PingCompleted,
            ..
        }
    )));
}

fn launch_result_for_summary(
    status: LaunchProcessStatus,
    pid: Option<u32>,
    exit_code: Option<i32>,
) -> LaunchProcessResult {
    LaunchProcessResult {
        status,
        error: Some("sensitive operating-system detail".to_string()),
        pid,
        exit_code,
        stdout: Some("sensitive stdout".to_string()),
        stderr: Some("sensitive stderr".to_string()),
        stdout_file: Some("stdout file".to_string()),
        stderr_file: Some("stderr file".to_string()),
    }
}

#[test]
fn launch_process_summaries_are_stable_and_concise() {
    let cases = [
        (
            LaunchProcessStatus::Completed,
            Some(123),
            Some(0),
            "Process 123 completed with exit code 0.",
        ),
        (
            LaunchProcessStatus::Completed,
            Some(123),
            Some(7),
            "Process 123 completed with exit code 7.",
        ),
        (
            LaunchProcessStatus::Completed,
            Some(123),
            None,
            "Process 123 completed.",
        ),
        (
            LaunchProcessStatus::Completed,
            None,
            None,
            "Process completed.",
        ),
        (
            LaunchProcessStatus::Detached,
            Some(123),
            None,
            "Process 123 started and was detached.",
        ),
        (
            LaunchProcessStatus::DetachedWithStopTimeout,
            Some(123),
            None,
            "Process 123 started detached with a stop timeout.",
        ),
        (
            LaunchProcessStatus::TimedOutDetached,
            Some(123),
            None,
            "Process 123 timed out and was detached.",
        ),
        (
            LaunchProcessStatus::TimedOutStopped,
            Some(123),
            None,
            "Process 123 timed out and was stopped.",
        ),
        (
            LaunchProcessStatus::SetupFailed,
            None,
            None,
            "Process setup failed.",
        ),
        (
            LaunchProcessStatus::LaunchProcessFailed,
            None,
            None,
            "Process launch failed.",
        ),
        (
            LaunchProcessStatus::WaitFailed,
            Some(123),
            None,
            "Waiting for process 123 failed.",
        ),
        (
            LaunchProcessStatus::WaitFailed,
            None,
            None,
            "Waiting for the process failed.",
        ),
        (
            LaunchProcessStatus::StopFailed,
            Some(123),
            None,
            "Stopping process 123 failed; successful termination could not be confirmed.",
        ),
        (
            LaunchProcessStatus::StopFailed,
            None,
            None,
            "Stopping the process failed; successful termination could not be confirmed.",
        ),
    ];

    for (status, pid, exit_code, expected) in cases {
        let result = launch_result_for_summary(status, pid, exit_code);
        let summary = launch_process_summary(&result);
        assert_eq!(summary, expected);
        for sensitive in [
            "sensitive operating-system detail",
            "sensitive stdout",
            "sensitive stderr",
            "stdout file",
            "stderr file",
        ] {
            assert!(!summary.contains(sensitive));
        }
    }
}

#[test]
fn read_file_preserves_logical_lines_and_newline_semantics() {
    let path = write_temp_test_file("read_lines", b"\xEF\xBB\xBFfirst\r\n\r\nthird\nlast");
    let (call_result, events) = call_read_file_direct(make_read_file_request(&path, 1, 4));
    let result = read_file_structured_result(&call_result);

    assert_eq!(result.status, ReadFileStatus::Completed);
    assert_eq!(result.actual_start_line, Some(1));
    assert_eq!(result.actual_end_line, Some(4));
    assert_eq!(result.text, "first\n\nthird\nlast");
    assert_eq!(result.eof, Some(true));
    assert!(!result.lossy_utf8);
    assert_eq!(result.next_start_line, None);
    assert_eq!(call_result.is_error, Some(false));
    assert_eq!(only_text_content(&call_result), read_file_summary(&result));
    assert!(!only_text_content(&call_result).contains("third"));
    let UiEventKind::RequestStarted {
        id,
        request:
            RequestData::ReadFile {
                path: ref event_path,
                start_line: 1,
                end_line: 4,
            },
        ..
    } = events[0]
    else {
        panic!("expected read_file request start");
    };
    assert_eq!(event_path, &path.to_string_lossy().into_owned());
    assert!(matches!(
        events[1],
        UiEventKind::RequestUpdated {
            id: update_id,
            update: RequestUpdate::ReadFileResponded {
                status: ReadFileStatus::Completed,
                actual_start_line: Some(1),
                actual_end_line: Some(4),
                ..
            },
        } if update_id == id
    ));
    assert!(!format!("{events:?}").contains("third"));

    let (single_call, _) = call_read_file_direct(make_read_file_request(&path, 2, 2));
    let single = read_file_structured_result(&single_call);
    assert_eq!(single.text, "");
    assert_eq!(single.actual_start_line, Some(2));
    assert_eq!(single.actual_end_line, Some(2));
    assert_eq!(single.eof, Some(false));

    let leading_blanks_path = write_temp_test_file("leading_blanks", b"\n\nthird\n");
    let leading_blanks = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&leading_blanks_path, 1, 3)).0,
    );
    assert_eq!(leading_blanks.text, "\n\nthird");
    assert_eq!(leading_blanks.actual_start_line, Some(1));
    assert_eq!(leading_blanks.actual_end_line, Some(3));

    let bom_elsewhere = write_temp_test_file("bom_elsewhere", b"first\n\xEF\xBB\xBFsecond\n");
    let (bom_call, _) = call_read_file_direct(make_read_file_request(&bom_elsewhere, 2, 2));
    assert_eq!(
        read_file_structured_result(&bom_call).text,
        "\u{feff}second"
    );

    let lf_path = write_temp_test_file("lf", b"one\ntwo\n");
    let crlf_path = write_temp_test_file("crlf", b"one\r\ntwo\r\n");
    let lf = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&lf_path, 1, 2)).0,
    );
    let crlf = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&crlf_path, 1, 2)).0,
    );
    assert_eq!(lf.text, crlf.text);

    for path in [path, leading_blanks_path, bom_elsewhere, lf_path, crlf_path] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn read_file_handles_eof_empty_files_unicode_and_lossy_utf8() {
    let path = write_temp_test_file("eof", b"one\ntwo\nthree");

    let beyond = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&path, 20, 21)).0,
    );
    assert_eq!(beyond.status, ReadFileStatus::Completed);
    assert_eq!(beyond.actual_start_line, None);
    assert_eq!(beyond.actual_end_line, None);
    assert_eq!(beyond.text, "");
    assert_eq!(beyond.eof, Some(true));
    assert_eq!(beyond.next_start_line, None);

    let past_end =
        read_file_structured_result(&call_read_file_direct(make_read_file_request(&path, 2, 10)).0);
    assert_eq!(past_end.text, "two\nthree");
    assert_eq!(past_end.actual_end_line, Some(3));
    assert_eq!(past_end.eof, Some(true));

    let before_eof =
        read_file_structured_result(&call_read_file_direct(make_read_file_request(&path, 1, 2)).0);
    assert_eq!(before_eof.text, "one\ntwo");
    assert_eq!(before_eof.eof, Some(false));

    let empty_path = write_temp_test_file("empty", b"");
    let empty = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&empty_path, 1, 1)).0,
    );
    assert_eq!(empty.text, "");
    assert_eq!(empty.actual_start_line, None);
    assert_eq!(empty.eof, Some(true));

    let unicode_path = write_temp_test_file("unicode_雪", "雪\n🙂\n".as_bytes());
    let unicode = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&unicode_path, 1, 2)).0,
    );
    assert_eq!(unicode.text, "雪\n🙂");
    assert!(!unicode.lossy_utf8);

    let invalid_path = write_temp_test_file("invalid_utf8", b"valid\n\xFF\xFE\n");
    let invalid = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&invalid_path, 2, 2)).0,
    );
    assert_eq!(invalid.text, "\u{fffd}\u{fffd}");
    assert!(invalid.lossy_utf8);

    for path in [path, empty_path, unicode_path, invalid_path] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn empty_read_file_result_has_one_correlated_lifecycle_without_file_text() {
    let path = write_temp_test_file("empty_lifecycle", b"");
    let (call, events) = call_read_file_direct(make_read_file_request(&path, 1, 1));
    let result = read_file_structured_result(&call);
    assert_eq!(result.status, ReadFileStatus::Completed);
    assert_eq!(result.actual_start_line, None);
    assert_eq!(result.eof, Some(true));
    assert_eq!(events.len(), 2);
    let UiEventKind::RequestStarted {
        id,
        request: RequestData::ReadFile { .. },
        ..
    } = events[0]
    else {
        panic!("expected read_file start");
    };
    assert!(matches!(
        events[1],
        UiEventKind::RequestUpdated {
            id: update_id,
            update: RequestUpdate::ReadFileResponded {
                status: ReadFileStatus::Completed,
                actual_start_line: None,
                actual_end_line: None,
                eof: Some(true),
                ..
            },
        } if update_id == id
    ));
    assert!(!format!("{events:?}").contains("private file body"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn read_file_resolves_absolute_relative_and_parent_paths() {
    let absolute_path = write_temp_test_file("absolute", b"absolute\n");
    let absolute = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&absolute_path, 1, 1)).0,
    );
    assert!(Path::new(&absolute.path).is_absolute());
    assert_eq!(absolute.text, "absolute");

    let relative_name = generate_temp_test_path("relative")
        .file_name()
        .unwrap()
        .to_owned();
    let relative_path = std::env::temp_dir().join(&relative_name);
    std::fs::write(&relative_path, b"relative\n").unwrap();
    let relative_request = ReadFileRequest {
        path: PathBuf::from(&relative_name).to_string_lossy().into_owned(),
        start_line: 1,
        end_line: 1,
    };
    let relative = read_file_structured_result(&call_read_file_direct(relative_request).0);
    assert_eq!(relative.text, "relative");
    assert!(Path::new(&relative.path).is_absolute());

    let parent_name = generate_temp_test_path("parent")
        .file_name()
        .unwrap()
        .to_owned();
    let parent_path = std::env::temp_dir().join(&parent_name);
    std::fs::write(&parent_path, b"parent\n").unwrap();
    let subdir = generate_temp_test_path("parent_subdir");
    std::fs::create_dir(&subdir).unwrap();
    let parent_request = ReadFileRequest {
        path: PathBuf::from(subdir.file_name().unwrap())
            .join("..")
            .join(&parent_name)
            .to_string_lossy()
            .into_owned(),
        start_line: 1,
        end_line: 1,
    };
    let parent = read_file_structured_result(&call_read_file_direct(parent_request).0);
    assert_eq!(parent.text, "parent");

    std::fs::remove_file(absolute_path).unwrap();
    std::fs::remove_file(relative_path).unwrap();
    std::fs::remove_file(parent_path).unwrap();
    std::fs::remove_dir(subdir).unwrap();
}

#[test]
fn read_file_validates_ranges_and_ambiguous_windows_paths() {
    let valid_path = generate_temp_test_path("validation");
    for (start_line, end_line, message) in [
        (0, 1, "start_line"),
        (1, 0, "end_line"),
        (2, 1, "less than or equal"),
        (1, 501, "500"),
    ] {
        let req = make_read_file_request(&valid_path, start_line, end_line);
        let error = validate_read_file_request(&req).unwrap_err();
        assert!(error.contains(message), "unexpected error: {error}");
    }

    for path in ["", "bad\0path"] {
        let req = ReadFileRequest {
            path: path.to_string(),
            start_line: 1,
            end_line: 1,
        };
        assert!(validate_read_file_request(&req).is_err());
    }

    #[cfg(target_os = "windows")]
    for path in [r"C:some-file.txt", r"\some-file.txt"] {
        let req = ReadFileRequest {
            path: path.to_string(),
            start_line: 1,
            end_line: 1,
        };
        assert!(validate_read_file_request(&req).is_err());
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let invalid = make_read_file_request(&valid_path, 1, 501);
    let error = rt.block_on(async {
        server
            .read_file(rmcp::handler::server::wrapper::Parameters(invalid))
            .await
    });
    assert!(error.is_err());
    let started = rx.try_recv().unwrap().kind;
    let updated = rx.try_recv().unwrap().kind;
    let UiEventKind::RequestStarted {
        id,
        request: RequestData::ReadFile { .. },
        ..
    } = started
    else {
        panic!("expected rejected read_file to start");
    };
    assert!(matches!(
        updated,
        UiEventKind::RequestUpdated {
            id: update_id,
            update: RequestUpdate::Rejected { .. },
        } if update_id == id
    ));
}

#[test]
fn read_file_range_validation_handles_u64_boundaries() {
    let path = generate_temp_test_path("range_boundaries");

    let excessive = make_read_file_request(&path, 1, u64::MAX);
    let excessive_error = std::panic::catch_unwind(|| validate_read_file_request(&excessive))
        .expect("u64::MAX range validation must not panic")
        .unwrap_err();
    assert!(excessive_error.contains("500"));

    let exactly_500 = make_read_file_request(&path, 1, 500);
    assert!(validate_read_file_request(&exactly_500).is_ok());

    let lines_501 = make_read_file_request(&path, 1, 501);
    let lines_501_error = validate_read_file_request(&lines_501).unwrap_err();
    assert!(lines_501_error.contains("500"));

    let high_single_line = make_read_file_request(&path, u64::MAX, u64::MAX);
    assert!(validate_read_file_request(&high_single_line).is_ok());
}

#[test]
fn read_file_returns_structured_filesystem_failures() {
    let missing_path = generate_temp_test_path("missing");
    let missing_call = call_read_file_direct(make_read_file_request(&missing_path, 1, 1)).0;
    let missing = read_file_structured_result(&missing_call);
    assert_eq!(missing.status, ReadFileStatus::NotFound);
    assert_eq!(missing_call.is_error, Some(false));
    assert!(missing.error.is_some());
    assert!(missing.text.is_empty());
    assert!(!only_text_content(&missing_call).contains(missing.error.as_deref().unwrap()));

    let directory = generate_temp_test_path("directory");
    std::fs::create_dir(&directory).unwrap();
    let directory_call = call_read_file_direct(make_read_file_request(&directory, 1, 1)).0;
    let directory_result = read_file_structured_result(&directory_call);
    assert_eq!(directory_result.status, ReadFileStatus::NotAFile);
    assert_eq!(directory_call.is_error, Some(false));
    assert!(directory_result.text.is_empty());
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn read_file_validates_metadata_from_the_opened_handle() {
    let path = write_temp_test_file("opened_metadata", b"regular file\n");
    let req = make_read_file_request(&path, 1, 1);

    let file = open_regular_file_with_metadata(&req, &path, std::fs::File::metadata)
        .expect("ordinary regular file should be accepted");
    drop(file);

    let metadata_failure = open_regular_file_with_metadata(&req, &path, |_| {
        Err(std::io::Error::other(
            "injected opened-handle metadata failure",
        ))
    })
    .unwrap_err();
    assert_eq!(metadata_failure.status, ReadFileStatus::ReadFailed);
    assert!(
        metadata_failure
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected opened-handle metadata failure"))
    );

    let directory = generate_temp_test_path("opened_metadata_directory");
    std::fs::create_dir(&directory).unwrap();
    let directory_metadata = std::fs::metadata(&directory).unwrap();
    let swapped_object =
        open_regular_file_with_metadata(&req, &path, move |_| Ok(directory_metadata)).unwrap_err();
    assert_eq!(swapped_object.status, ReadFileStatus::NotAFile);

    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn read_file_enforces_complete_line_byte_limit_and_continuation() {
    let exact_path = write_temp_test_file("exact_limit", &vec![b'a'; 256 * 1024]);
    let exact_call = call_read_file_direct(make_read_file_request(&exact_path, 1, 1)).0;
    let exact = read_file_structured_result(&exact_call);
    assert_eq!(exact.status, ReadFileStatus::Completed);
    assert_eq!(exact.text.len(), 256 * 1024);

    let below_path = write_temp_test_file("below_limit", &vec![b'b'; 256 * 1024 - 1]);
    let below = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&below_path, 1, 1)).0,
    );
    assert_eq!(below.status, ReadFileStatus::Completed);

    let mut truncation_bytes = vec![b'c'; 200 * 1024];
    truncation_bytes.push(b'\n');
    truncation_bytes.extend(vec![b'd'; 100 * 1024]);
    truncation_bytes.push(b'\n');
    truncation_bytes.extend_from_slice(b"third\n");
    let truncated_path = write_temp_test_file("truncated", &truncation_bytes);
    let (truncated_call, truncated_events) =
        call_read_file_direct(make_read_file_request(&truncated_path, 1, 3));
    let truncated = read_file_structured_result(&truncated_call);
    assert_eq!(truncated.status, ReadFileStatus::Truncated);
    assert_eq!(truncated.actual_start_line, Some(1));
    assert_eq!(truncated.actual_end_line, Some(1));
    assert_eq!(truncated.next_start_line, Some(2));
    assert_eq!(truncated.eof, Some(false));
    assert_eq!(truncated.text.len(), 200 * 1024);
    assert_eq!(truncated_call.is_error, Some(false));
    assert!(!only_text_content(&truncated_call).contains(&"c".repeat(100)));
    assert!(matches!(
        truncated_events.as_slice(),
        [
            UiEventKind::RequestStarted {
                id,
                request: RequestData::ReadFile { .. },
                ..
            },
            UiEventKind::RequestUpdated {
                id: update_id,
                update: RequestUpdate::ReadFileResponded {
                    status: ReadFileStatus::Truncated,
                    actual_start_line: Some(1),
                    actual_end_line: Some(1),
                    next_start_line: Some(2),
                    eof: Some(false),
                    ..
                },
            },
        ] if id == update_id
    ));

    let continued = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&truncated_path, 2, 3)).0,
    );
    assert_eq!(continued.status, ReadFileStatus::Completed);
    assert_eq!(continued.actual_start_line, Some(2));
    assert_eq!(continued.actual_end_line, Some(3));
    assert!(continued.text.starts_with(&"d".repeat(100)));
    assert!(continued.text.ends_with("\nthird"));

    let oversized_path = write_temp_test_file("oversized", &vec![b'e'; 256 * 1024 + 1]);
    let oversized_call = call_read_file_direct(make_read_file_request(&oversized_path, 1, 1)).0;
    let oversized = read_file_structured_result(&oversized_call);
    assert_eq!(oversized.status, ReadFileStatus::LineTooLong);
    assert_eq!(oversized.actual_start_line, None);
    assert_eq!(oversized.actual_end_line, None);
    assert!(oversized.text.is_empty());
    assert_eq!(oversized.next_start_line, None);
    assert!(oversized.error.as_deref().unwrap().contains("Line 1"));
    assert_eq!(oversized_call.is_error, Some(false));

    let mut blank_then_oversized_bytes = vec![b'\n'];
    blank_then_oversized_bytes.extend(vec![b'f'; 256 * 1024 + 1]);
    let blank_then_oversized_path =
        write_temp_test_file("blank_then_oversized", &blank_then_oversized_bytes);
    let blank_then_oversized = read_file_structured_result(
        &call_read_file_direct(make_read_file_request(&blank_then_oversized_path, 1, 2)).0,
    );
    assert_eq!(blank_then_oversized.status, ReadFileStatus::Truncated);
    assert_eq!(blank_then_oversized.actual_start_line, Some(1));
    assert_eq!(blank_then_oversized.actual_end_line, Some(1));
    assert_eq!(blank_then_oversized.text, "");
    assert_eq!(blank_then_oversized.next_start_line, Some(2));

    for path in [
        exact_path,
        below_path,
        truncated_path,
        oversized_path,
        blank_then_oversized_path,
    ] {
        std::fs::remove_file(path).unwrap();
    }
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

    let output_schema = attr
        .output_schema
        .as_ref()
        .expect("ping output schema should be present");
    let output_schema = rmcp::serde_json::Value::Object((**output_schema).clone());
    let output_schema = resolve_local_schema_ref(&output_schema, &output_schema);
    assert_eq!(
        output_schema.get("type").and_then(|value| value.as_str()),
        Some("object")
    );

    let message_schema = output_schema
        .get("properties")
        .and_then(|value| value.get("message"))
        .expect("ping output schema should contain message");
    let message_schema = resolve_local_schema_ref(output_schema, message_schema);
    assert_eq!(
        message_schema.get("type").and_then(|value| value.as_str()),
        Some("string")
    );
    assert!(
        output_schema
            .get("required")
            .and_then(|value| value.as_array())
            .is_some_and(|required| required.iter().any(|value| value == "message"))
    );
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
        assert_eq!(tools.len(), 3);
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

        let output_schema = tool
            .output_schema
            .as_ref()
            .expect("ping output schema should be present");
        let output_schema = rmcp::serde_json::Value::Object((**output_schema).clone());
        let output_schema = resolve_local_schema_ref(&output_schema, &output_schema);
        assert_eq!(
            output_schema.get("type").and_then(|value| value.as_str()),
            Some("object")
        );

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
        assert_eq!(call_result.is_error, Some(false));
        let structured_content = call_result
            .structured_content
            .clone()
            .expect("ping should return structured content");
        assert_eq!(
            structured_content,
            rmcp::serde_json::json!({ "message": "pong" })
        );
        let typed_result: PingResult = rmcp::serde_json::from_value(structured_content)
            .expect("ping structured content should match PingResult");
        assert_eq!(typed_result.message, "pong");

        // 5. Graceful client/server shutdown
        client.close().await.expect("Failed to close client");
        server_task.await.expect("Server task panicked");
    });

    // 6. UI lifecycle and tool events
    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert!(events.windows(2).any(|pair| matches!(
        pair,
        [
            UiEventKind::RequestStarted {
                id,
                request: RequestData::Ping,
                ..
            },
            UiEventKind::RequestUpdated {
                id: update_id,
                update: RequestUpdate::PingCompleted,
            },
        ] if id == update_id
    )));

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
        req.arguments = Some("some\0args".to_string());
        assert!(validate_request(&req).is_err());
    }

    // 5. Null character in an argument-array item, under cfg(not(windows))
    #[cfg(not(target_os = "windows"))]
    {
        let mut req = base_req.clone();
        req.arguments = Some(vec!["arg1".to_string(), "arg\0two".to_string()]);
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

    // Optional arguments validation
    // A. None is valid
    let mut req = base_req.clone();
    req.arguments = None;
    assert!(validate_request(&req).is_ok());

    // B. Empty string/vector is valid
    let mut req = base_req.clone();
    #[cfg(target_os = "windows")]
    {
        req.arguments = Some("".to_string());
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = Some(vec![]);
    }
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

    assert!(!args_schema.contains_key("default"));

    #[cfg(target_os = "windows")]
    {
        assert_eq!(
            args_schema.get("type").and_then(|value| value.as_str()),
            Some("string")
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(
            args_schema.get("type").and_then(|value| value.as_str()),
            Some("array")
        );
        assert_eq!(
            args_schema
                .get("items")
                .and_then(|value| value.get("type"))
                .and_then(|value| value.as_str()),
            Some("string")
        );
    }
}

#[test]
fn test_schema_required_fields() {
    let attr = McpServer::launch_process_tool_attr();
    let required = attr
        .input_schema
        .get("required")
        .unwrap()
        .as_array()
        .unwrap();

    let required_fields: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();

    // arguments must NOT be in required
    assert!(!required_fields.contains(&"arguments"));

    // process_name, environment, detached must be in required
    assert!(required_fields.contains(&"process_name"));
    assert!(required_fields.contains(&"environment"));
    assert!(required_fields.contains(&"detached"));
}

#[test]
fn launch_process_output_schema_remains_complete() {
    let attr = McpServer::launch_process_tool_attr();
    let schema = attr
        .output_schema
        .as_ref()
        .expect("launch_process output schema should be present");
    let schema = rmcp::serde_json::Value::Object((**schema).clone());
    let root = resolve_local_schema_ref(&schema, &schema);
    let properties = root["properties"].as_object().unwrap();
    for field in [
        "status",
        "error",
        "pid",
        "exit_code",
        "stdout",
        "stderr",
        "stdout_file",
        "stderr_file",
    ] {
        assert!(
            properties.contains_key(field),
            "missing output field {field}"
        );
    }

    let pid_schema = resolve_local_schema_ref(&schema, &properties["pid"]);
    assert!(pid_schema.get("format").is_none());
    let pid_integer_schema = pid_schema
        .get("anyOf")
        .and_then(|value| value.as_array())
        .and_then(|schemas| schemas.iter().find(|schema| schema["type"] == "integer"))
        .unwrap_or(pid_schema);
    let pid_types = pid_schema
        .get("type")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        pid_integer_schema["type"] == "integer"
            || pid_types
                .iter()
                .any(|value| value.as_str() == Some("integer"))
    );
    assert!(
        pid_schema["type"] == "null"
            || pid_types.iter().any(|value| value.as_str() == Some("null"))
            || pid_schema
                .get("anyOf")
                .and_then(|value| value.as_array())
                .is_some_and(|schemas| schemas.iter().any(|schema| schema["type"] == "null"))
    );
    assert_eq!(pid_integer_schema["minimum"], 0);
    assert_eq!(
        pid_integer_schema["maximum"],
        rmcp::serde_json::json!(u32::MAX)
    );

    let encoded = schema.to_string();
    assert!(!encoded.contains("\"format\":\"uint32\""));
    assert!(
        !root["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|field| field == "pid"))
    );
    for status in [
        "completed",
        "detached",
        "detached_with_stop_timeout",
        "timed_out_detached",
        "timed_out_stopped",
        "setup_failed",
        "launch_process_failed",
        "wait_failed",
        "stop_failed",
    ] {
        assert!(encoded.contains(status), "missing status {status}");
    }
}

#[test]
fn test_environment_schema_default_and_required_fields() {
    let attr = McpServer::launch_process_tool_attr();
    let root = rmcp::serde_json::Value::Object((*attr.input_schema).clone());
    let properties = root
        .get("properties")
        .and_then(|value| value.as_object())
        .unwrap();
    let environment_schema = resolve_local_schema_ref(&root, &properties["environment"]);
    let environment_properties = environment_schema["properties"].as_object().unwrap();
    let inherit_schema = resolve_local_schema_ref(&root, &environment_properties["inherit"]);

    assert_eq!(
        inherit_schema.get("type").and_then(|value| value.as_str()),
        Some("boolean")
    );
    assert_eq!(
        inherit_schema.get("default"),
        Some(&rmcp::serde_json::Value::Bool(true))
    );

    let top_level_required = root["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        top_level_required,
        ["detached", "environment", "process_name"]
            .into_iter()
            .collect()
    );

    let environment_required = environment_schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(environment_required, ["variables"].into_iter().collect());
    assert!(!environment_required.contains("inherit"));
    assert!(
        !properties["arguments"]
            .as_object()
            .unwrap()
            .contains_key("default")
    );
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
fn test_successful_completion_without_arguments() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let req = LaunchProcessRequest {
        working_directory: None,
        process_name: make_helper_request().process_name,
        arguments: None,
        environment: EnvironmentConfig {
            inherit: true,
            variables: std::collections::HashMap::new(),
        },
        detached: false,
        timeout_ms: None,
        timeout_action: None,
    };

    assert!(validate_request(&req).is_ok());
    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.exit_code, Some(0));
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
    let nonexistent_path = generate_temp_test_path("nonexistent_working_directory");
    assert!(!nonexistent_path.exists());
    let mut req = make_helper_request();
    req.working_directory = Some(nonexistent_path.to_string_lossy().into_owned());
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
    assert_eq!(res.stdout.as_deref(), Some("STDIN_EOF"));
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

    // A naturally exiting detached child is reaped as an exit, not as a timeout.
    let natural_marker_path = generate_temp_test_path("det_stop_natural_marker");
    let mut req = make_helper_request();
    req.detached = true;
    req.timeout_ms = Some(2000);
    req.timeout_action = Some(TimeoutAction::Stop);
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_ACTION".to_string(),
        Some("sleep".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_SLEEP_MS".to_string(),
        Some("100".to_string()),
    );
    req.environment.variables.insert(
        "RMCP_TEST_HELPER_MARKER".to_string(),
        Some(natural_marker_path.to_string_lossy().into_owned()),
    );

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(
        res.status,
        LaunchProcessStatus::DetachedWithStopTimeout
    ));
    let natural_pid = res.pid.unwrap();
    let completed_pid = completion_rx
        .recv_timeout(Duration::from_millis(5000))
        .unwrap();
    assert_eq!(completed_pid, natural_pid);
    assert!(natural_marker_path.exists());
    let _ = std::fs::remove_file(natural_marker_path);
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
    let structured: LaunchProcessResult =
        rmcp::serde_json::from_value(res.structured_content.clone().expect("structured result"))
            .unwrap();
    assert!(matches!(structured.status, LaunchProcessStatus::Completed));

    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert_eq!(events.len(), 2);
    let UiEventKind::RequestStarted {
        id,
        request: RequestData::LaunchProcess { ref command_line },
        ..
    } = events[0]
    else {
        panic!("expected launch_process request start");
    };
    assert_eq!(command_line, &make_helper_request().process_name);
    assert!(matches!(
        events[1],
        UiEventKind::RequestUpdated {
            id: update_id,
            update: RequestUpdate::LaunchProcessResponded {
                status: LaunchProcessStatus::Completed,
                pid,
                exit_code: Some(0),
                ..
            },
        } if update_id == id && pid == structured.pid
    ));

    let (tx2, rx2) = std::sync::mpsc::channel();
    let server2 = McpServer::new(tx2, Instant::now());

    let params = rmcp::handler::server::wrapper::Parameters(LaunchProcessRequest {
        working_directory: None,
        process_name: "".to_string(),
        #[cfg(target_os = "windows")]
        arguments: Some("".to_string()),
        #[cfg(not(target_os = "windows"))]
        arguments: Some(vec![]),
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
    assert_eq!(events2.len(), 2);
    let UiEventKind::RequestStarted {
        id: rejected_id,
        request: RequestData::LaunchProcess { .. },
        ..
    } = events2[0]
    else {
        panic!("expected rejected launch_process to start");
    };
    assert!(matches!(
        events2[1],
        UiEventKind::RequestUpdated {
            id,
            update: RequestUpdate::Rejected { .. },
        } if id == rejected_id
    ));
}

#[test]
fn launch_process_events_include_command_line_but_exclude_environment_and_output() {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = McpServer::new(tx, Instant::now());
    let mut variables = std::collections::HashMap::new();
    variables.insert(
        "SECRET_ENV_NAME".to_string(),
        Some("secret value".to_string()),
    );
    let request = LaunchProcessRequest {
        working_directory: None,
        process_name: "safe-process-name".to_string(),
        #[cfg(target_os = "windows")]
        arguments: Some("secret argument".to_string()),
        #[cfg(not(target_os = "windows"))]
        arguments: Some(vec!["secret argument".to_string()]),
        environment: EnvironmentConfig {
            inherit: true,
            variables,
        },
        detached: false,
        timeout_ms: Some(1),
        timeout_action: None,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(
        rt.block_on(server.launch_process(rmcp::handler::server::wrapper::Parameters(request)))
            .is_err()
    );
    let events = rx.try_iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    let debug = format!("{events:?}");
    assert!(debug.contains("safe-process-name secret argument"));
    for sensitive in [
        "SECRET_ENV_NAME",
        "secret value",
        "private stdout",
        "private stderr",
    ] {
        assert!(!debug.contains(sensitive));
    }
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

    let inherited_name = "RMCP_TEST_MCP_INHERITED";
    unsafe {
        std::env::set_var(inherited_name, "inherited through MCP");
    }

    rt.block_on(async {
        let tx_clone = tx.clone();
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx_clone, start_time, server_transport).await;
        });

        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("Failed to serve client");

        // 1. Tool discovery integration test
        let tools = client.list_all_tools().await.expect("Failed to list tools");
        assert_eq!(tools.len(), 3);

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

        let output_schema = launch_tool
            .output_schema
            .as_ref()
            .expect("launch_process output schema should be present");
        let output_schema = rmcp::serde_json::Value::Object((**output_schema).clone());
        let output_root = resolve_local_schema_ref(&output_schema, &output_schema);
        assert!(output_root["properties"].get("stdout").is_some());
        assert!(output_root["properties"].get("stderr").is_some());

        let properties = launch_tool
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        let args_schema = properties.get("arguments").unwrap().as_object().unwrap();
        assert!(!args_schema.contains_key("default"));
        #[cfg(target_os = "windows")]
        {
            assert_eq!(
                args_schema.get("type").and_then(|value| value.as_str()),
                Some("string")
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(
                args_schema.get("type").and_then(|value| value.as_str()),
                Some("array")
            );
            assert_eq!(
                args_schema
                    .get("items")
                    .and_then(|value| value.get("type"))
                    .and_then(|value| value.as_str()),
                Some("string")
            );
        }
        let required = launch_tool
            .input_schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap();
        let required_fields: Vec<&str> =
            required.iter().filter_map(|value| value.as_str()).collect();
        assert!(!required_fields.contains(&"arguments"));
        assert!(required_fields.contains(&"process_name"));
        assert!(required_fields.contains(&"environment"));
        assert!(required_fields.contains(&"detached"));

        let schema_root = rmcp::serde_json::Value::Object((*launch_tool.input_schema).clone());
        let environment_schema =
            resolve_local_schema_ref(&schema_root, &schema_root["properties"]["environment"]);
        let inherit_schema =
            resolve_local_schema_ref(&schema_root, &environment_schema["properties"]["inherit"]);
        assert_eq!(
            inherit_schema.get("default"),
            Some(&rmcp::serde_json::Value::Bool(true))
        );

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
        let base_arguments_val = rmcp::serde_json::json!(escape_windows_args(&["integration_arg"]));
        #[cfg(not(target_os = "windows"))]
        let base_arguments_val = rmcp::serde_json::json!(vec!["integration_arg".to_string()]);

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

        assert_eq!(call_result.is_error, Some(false));
        let summary = only_text_content(&call_result).to_string();
        assert!(summary.starts_with("Process "));
        assert!(summary.ends_with(" completed with exit code 0."));
        for sensitive in [
            "stdout: integration_test",
            "stderr: integration_test",
            "integration_arg",
            "RMCP_TEST_HELPER_ACTION",
        ] {
            assert!(!summary.contains(sensitive));
        }
        assert!(!summary.starts_with('{'));

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

        // 2. Omitted arguments are accepted through the real MCP interface.
        let mut no_arguments_params = rmcp::model::CallToolRequestParams::new("launch_process");
        no_arguments_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": make_helper_request().process_name,
                "environment": {
                    "inherit": true,
                    "variables": {}
                },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let no_arguments_result = client
            .call_tool(no_arguments_params)
            .await
            .expect("launch_process should accept omitted arguments");
        let no_arguments_structured = no_arguments_result
            .structured_content
            .expect("Expected structured no-arguments result");
        let no_arguments_result: LaunchProcessResult =
            rmcp::serde_json::from_value(no_arguments_structured).unwrap();
        assert!(matches!(
            no_arguments_result.status,
            LaunchProcessStatus::Completed
        ));
        assert_eq!(no_arguments_result.exit_code, Some(0));

        // 3. Omitted inherit defaults to true through tools/call.
        let mut omitted_inherit_params = rmcp::model::CallToolRequestParams::new("launch_process");
        omitted_inherit_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": make_helper_request().process_name,
                "environment": {
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "env",
                        "RMCP_TEST_HELPER_ENV_NAME": inherited_name
                    }
                },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let omitted_inherit_result = client
            .call_tool(omitted_inherit_params)
            .await
            .expect("launch_process should default omitted inherit to true");
        let omitted_inherit_result: LaunchProcessResult = rmcp::serde_json::from_value(
            omitted_inherit_result
                .structured_content
                .expect("Expected structured omitted-inherit result"),
        )
        .unwrap();
        assert_eq!(
            omitted_inherit_result.stdout.as_deref(),
            Some("inherited through MCP")
        );

        // 4. Explicit false clears inherited values after applying the supplied
        // helper action and queried-variable name.
        let mut no_inherit_params = rmcp::model::CallToolRequestParams::new("launch_process");
        no_inherit_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": make_helper_request().process_name,
                "environment": {
                    "inherit": false,
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "env",
                        "RMCP_TEST_HELPER_ENV_NAME": inherited_name
                    }
                },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let no_inherit_result = client
            .call_tool(no_inherit_params)
            .await
            .expect("launch_process should accept explicit false inherit");
        let no_inherit_result: LaunchProcessResult = rmcp::serde_json::from_value(
            no_inherit_result
                .structured_content
                .expect("Expected structured no-inherit result"),
        )
        .unwrap();
        assert!(matches!(
            no_inherit_result.status,
            LaunchProcessStatus::Completed
        ));
        assert_eq!(no_inherit_result.stdout.as_deref(), Some(""));

        // 5. Validation-error integration test
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

        // 6. Graceful client/server shutdown
        client.close().await.expect("Failed to close client");
        server_task.await.expect("Server task panicked");
    });

    unsafe {
        std::env::remove_var(inherited_name);
    }

    // 7. Verify correlated GUI request lifecycles.
    let events: Vec<UiEventKind> = rx.try_iter().map(|e| e.kind).collect();
    assert!(events.windows(2).any(|pair| matches!(
        pair,
        [
            UiEventKind::RequestStarted {
                id,
                request: RequestData::LaunchProcess { .. },
                ..
            },
            UiEventKind::RequestUpdated {
                id: update_id,
                update: RequestUpdate::LaunchProcessResponded {
                    status: LaunchProcessStatus::Completed,
                    exit_code: Some(0),
                    ..
                },
            },
        ] if id == update_id
    )));
    assert!(events.windows(2).any(|pair| matches!(
        pair,
        [
            UiEventKind::RequestStarted {
                id,
                request: RequestData::LaunchProcess { .. },
                ..
            },
            UiEventKind::RequestUpdated {
                id: update_id,
                update: RequestUpdate::Rejected { error },
            },
        ] if id == update_id && error == "timeout_ms requires timeout_action"
    )));
    assert!(events.windows(2).any(|pair| matches!(
        pair,
        [
            UiEventKind::RequestStarted {
                id,
                request: RequestData::Ping,
                ..
            },
            UiEventKind::RequestUpdated {
                id: update_id,
                update: RequestUpdate::PingCompleted,
            },
        ] if id == update_id
    )));
    assert!(matches!(events.last(), Some(UiEventKind::ServerStopped)));
}

#[test]
fn launch_process_mcp_summaries_cover_nonzero_detach_timeouts_and_failure() {
    let _guard = match ENV_MUTEX.lock() {
        Ok(guard) => guard,
        Err(error) => error.into_inner(),
    };
    let helper = make_helper_request().process_name;
    let (tx, rx) = std::sync::mpsc::channel();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx, Instant::now(), server_transport).await;
        });
        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("serve client");

        let mut nonzero_params = rmcp::model::CallToolRequestParams::new("launch_process");
        nonzero_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": &helper,
                "environment": {
                    "inherit": true,
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "exit_code",
                        "RMCP_TEST_HELPER_CODE": "7"
                    }
                },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let nonzero_call = client
            .call_tool(nonzero_params)
            .await
            .expect("nonzero launch");
        let nonzero: LaunchProcessResult =
            rmcp::serde_json::from_value(nonzero_call.structured_content.clone().unwrap()).unwrap();
        assert_eq!(nonzero.exit_code, Some(7));
        assert_eq!(nonzero_call.is_error, Some(false));
        assert_eq!(
            only_text_content(&nonzero_call),
            launch_process_summary(&nonzero)
        );

        let mut detached_params = rmcp::model::CallToolRequestParams::new("launch_process");
        detached_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": &helper,
                "environment": {
                    "inherit": true,
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "sleep",
                        "RMCP_TEST_HELPER_SLEEP_MS": "300"
                    }
                },
                "detached": true
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let detached_call = client
            .call_tool(detached_params)
            .await
            .expect("detached launch");
        let detached: LaunchProcessResult =
            rmcp::serde_json::from_value(detached_call.structured_content.clone().unwrap())
                .unwrap();
        assert_eq!(detached.status, LaunchProcessStatus::Detached);
        assert_eq!(detached_call.is_error, Some(false));
        assert_eq!(
            only_text_content(&detached_call),
            launch_process_summary(&detached)
        );

        let mut timeout_detach_params = rmcp::model::CallToolRequestParams::new("launch_process");
        timeout_detach_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": &helper,
                "environment": {
                    "inherit": true,
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "sleep",
                        "RMCP_TEST_HELPER_SLEEP_MS": "400"
                    }
                },
                "detached": false,
                "timeout_ms": 50,
                "timeout_action": "detach"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let timeout_detach_call = client
            .call_tool(timeout_detach_params)
            .await
            .expect("timeout detach");
        let timeout_detach: LaunchProcessResult =
            rmcp::serde_json::from_value(timeout_detach_call.structured_content.clone().unwrap())
                .unwrap();
        assert_eq!(timeout_detach.status, LaunchProcessStatus::TimedOutDetached);
        assert_eq!(timeout_detach_call.is_error, Some(false));
        assert_eq!(
            only_text_content(&timeout_detach_call),
            launch_process_summary(&timeout_detach)
        );

        let mut timeout_stop_params = rmcp::model::CallToolRequestParams::new("launch_process");
        timeout_stop_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": &helper,
                "environment": {
                    "inherit": true,
                    "variables": {
                        "RMCP_TEST_HELPER_ACTION": "sleep",
                        "RMCP_TEST_HELPER_SLEEP_MS": "400"
                    }
                },
                "detached": false,
                "timeout_ms": 50,
                "timeout_action": "stop"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let timeout_stop_call = client
            .call_tool(timeout_stop_params)
            .await
            .expect("timeout stop");
        let timeout_stop: LaunchProcessResult =
            rmcp::serde_json::from_value(timeout_stop_call.structured_content.clone().unwrap())
                .unwrap();
        assert_eq!(timeout_stop.status, LaunchProcessStatus::TimedOutStopped);
        assert_eq!(timeout_stop_call.is_error, Some(false));
        assert_eq!(
            only_text_content(&timeout_stop_call),
            launch_process_summary(&timeout_stop)
        );

        let mut failure_params = rmcp::model::CallToolRequestParams::new("launch_process");
        failure_params.arguments = Some(
            rmcp::serde_json::json!({
                "process_name": generate_temp_test_path("missing_executable").to_string_lossy(),
                "environment": { "inherit": true, "variables": {} },
                "detached": false
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let failure_call = client
            .call_tool(failure_params)
            .await
            .expect("structured launch failure");
        let failure: LaunchProcessResult =
            rmcp::serde_json::from_value(failure_call.structured_content.clone().unwrap()).unwrap();
        assert_eq!(failure.status, LaunchProcessStatus::LaunchProcessFailed);
        assert_eq!(failure_call.is_error, Some(false));
        assert_eq!(only_text_content(&failure_call), "Process launch failed.");
        assert!(!only_text_content(&failure_call).contains(failure.error.as_deref().unwrap()));

        // Keep the process-test mutex until both deliberately detached helper
        // children have exited and their reapers have sent any test notifications.
        tokio::time::sleep(Duration::from_millis(600)).await;

        client.close().await.expect("close client");
        server_task.await.expect("server task");
    });

    let events = rx.try_iter().map(|event| event.kind).collect::<Vec<_>>();
    for status in [
        LaunchProcessStatus::Completed,
        LaunchProcessStatus::Detached,
        LaunchProcessStatus::TimedOutDetached,
        LaunchProcessStatus::TimedOutStopped,
        LaunchProcessStatus::LaunchProcessFailed,
    ] {
        assert!(
            events.iter().any(|event| matches!(
                event,
                UiEventKind::RequestUpdated {
                    update: RequestUpdate::LaunchProcessResponded {
                        status: event_status,
                        ..
                    },
                    ..
                } if *event_status == status
            )),
            "missing GUI update for {status:?}: {events:?}"
        );
    }
    assert!(events.iter().any(|event| matches!(
        event,
        UiEventKind::RequestUpdated {
            update: RequestUpdate::LaunchProcessResponded {
                status: LaunchProcessStatus::Completed,
                exit_code: Some(7),
                ..
            },
            ..
        }
    )));

    for (index, event) in events.iter().enumerate() {
        if let UiEventKind::RequestUpdated { id, .. } = event {
            assert!(events[..index].iter().any(|prior| matches!(
                prior,
                UiEventKind::RequestStarted { id: started_id, .. } if started_id == id
            )));
        }
    }
}

#[test]
fn read_file_integration_test_over_duplex() {
    let relative_name = generate_temp_test_path("mcp_read_relative")
        .file_name()
        .unwrap()
        .to_owned();
    let relative_path = std::env::temp_dir().join(&relative_name);
    std::fs::write(&relative_path, b"alpha\nbeta\ngamma\n").unwrap();

    let mut truncation_bytes = vec![b'x'; 200 * 1024];
    truncation_bytes.push(b'\n');
    truncation_bytes.extend(vec![b'y'; 100 * 1024]);
    let truncated_path = write_temp_test_file("mcp_read_truncated", &truncation_bytes);
    let missing_path = generate_temp_test_path("mcp_read_missing");

    let (tx, rx) = std::sync::mpsc::channel();
    let (server_transport, client_transport) = tokio::io::duplex(1024 * 1024);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx, Instant::now(), server_transport).await;
        });

        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("serve client");
        let tools = client.list_all_tools().await.expect("list tools");
        assert_eq!(tools.len(), 3);
        let tool = tools
            .iter()
            .find(|tool| tool.name == "read_file")
            .expect("read_file tool should be exposed");

        let annotations = tool.annotations.as_ref().expect("read_file annotations");
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(false));

        let required = tool.input_schema["required"].as_array().unwrap();
        for field in ["path", "start_line", "end_line"] {
            assert!(required.iter().any(|value| value == field));
        }
        let input_properties = tool.input_schema["properties"].as_object().unwrap();
        for field in ["start_line", "end_line"] {
            let schema = &input_properties[field];
            assert_eq!(schema["type"], "integer");
            assert_eq!(schema["minimum"], 1);
            assert!(schema.get("format").is_none());
            assert!(schema.get("default").is_none());
        }
        let encoded_input_schema =
            rmcp::serde_json::Value::Object((*tool.input_schema).clone()).to_string();
        assert!(!encoded_input_schema.contains("\"default\":null"));

        let output_schema = tool
            .output_schema
            .as_ref()
            .expect("read_file output schema should be present");
        let output_schema = rmcp::serde_json::Value::Object((**output_schema).clone());
        let output_root = resolve_local_schema_ref(&output_schema, &output_schema);
        let output_properties = output_root["properties"].as_object().unwrap();
        for field in [
            "status",
            "error",
            "path",
            "requested_start_line",
            "requested_end_line",
            "actual_start_line",
            "actual_end_line",
            "text",
            "eof",
            "next_start_line",
            "lossy_utf8",
        ] {
            assert!(
                output_properties.contains_key(field),
                "missing output field {field}"
            );
        }
        let encoded_output_schema = output_schema.to_string();
        for status in [
            "completed",
            "truncated",
            "not_found",
            "access_denied",
            "not_a_file",
            "read_failed",
            "line_too_long",
        ] {
            assert!(encoded_output_schema.contains(status));
        }
        assert!(!encoded_output_schema.contains("\"default\":null"));

        let mut completed_params = rmcp::model::CallToolRequestParams::new("read_file");
        completed_params.arguments = Some(
            rmcp::serde_json::json!({
                "path": PathBuf::from(&relative_name).to_string_lossy(),
                "start_line": 2,
                "end_line": 3
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let completed_call = client
            .call_tool(completed_params)
            .await
            .expect("read relative file");
        assert_eq!(completed_call.is_error, Some(false));
        assert_eq!(completed_call.content.len(), 1);
        let completed = read_file_structured_result(&completed_call);
        assert_eq!(completed.status, ReadFileStatus::Completed);
        assert_eq!(completed.text, "beta\ngamma");
        assert!(!only_text_content(&completed_call).contains("beta"));

        let mut truncated_params = rmcp::model::CallToolRequestParams::new("read_file");
        truncated_params.arguments = Some(
            rmcp::serde_json::json!({
                "path": truncated_path.to_string_lossy(),
                "start_line": 1,
                "end_line": 2
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let truncated_call = client
            .call_tool(truncated_params)
            .await
            .expect("truncated result");
        assert_eq!(truncated_call.is_error, Some(false));
        assert_eq!(
            read_file_structured_result(&truncated_call).status,
            ReadFileStatus::Truncated
        );

        let mut missing_params = rmcp::model::CallToolRequestParams::new("read_file");
        missing_params.arguments = Some(
            rmcp::serde_json::json!({
                "path": missing_path.to_string_lossy(),
                "start_line": 1,
                "end_line": 1
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let missing_call = client
            .call_tool(missing_params)
            .await
            .expect("structured missing-file result");
        assert_eq!(missing_call.is_error, Some(false));
        assert_eq!(
            read_file_structured_result(&missing_call).status,
            ReadFileStatus::NotFound
        );

        let excessive_arguments = rmcp::serde_json::json!({
            "path": relative_path.to_string_lossy(),
            "start_line": 1,
            "end_line": u64::MAX
        });
        assert_eq!(excessive_arguments["end_line"].as_u64(), Some(u64::MAX));
        let deserialised: ReadFileRequest =
            rmcp::serde_json::from_value(excessive_arguments.clone()).unwrap();
        assert_eq!(deserialised.end_line, u64::MAX);

        let mut invalid_params = rmcp::model::CallToolRequestParams::new("read_file");
        invalid_params.arguments = Some(excessive_arguments.as_object().unwrap().clone());
        let invalid_error = client.call_tool(invalid_params).await.unwrap_err();
        let rmcp::ServiceError::McpError(error) = invalid_error else {
            panic!("expected MCP invalid-parameter error");
        };
        assert_eq!(error.code.0, -32602);
        assert!(error.message.contains("500"));

        let ping = client
            .call_tool(rmcp::model::CallToolRequestParams::new("ping"))
            .await
            .expect("ping should remain responsive after rejected u64::MAX range");
        assert_eq!(only_text_content(&ping), "pong");

        let events: Vec<_> = rx.try_iter().map(|event| event.kind).collect();
        let rejected_id = events.iter().find_map(|event| match event {
            UiEventKind::RequestUpdated {
                id,
                update: RequestUpdate::Rejected { error },
            } if error.contains("500") => Some(*id),
            _ => None,
        });
        let rejected_id = rejected_id.expect("expected rejected read_file update");
        assert!(events.iter().any(|event| matches!(
            event,
            UiEventKind::RequestStarted {
                id,
                request: RequestData::ReadFile { .. },
                ..
            } if *id == rejected_id
        )));
        for status in [
            ReadFileStatus::Completed,
            ReadFileStatus::Truncated,
            ReadFileStatus::NotFound,
        ] {
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    UiEventKind::RequestUpdated {
                        update: RequestUpdate::ReadFileResponded {
                            status: event_status,
                            ..
                        },
                        ..
                    } if *event_status == status
                )),
                "missing read_file GUI update for {status:?}: {events:?}"
            );
        }
        assert!(!format!("{events:?}").contains("beta\ngamma"));

        client.close().await.expect("close client");
        server_task.await.expect("server task");
    });

    std::fs::remove_file(relative_path).unwrap();
    std::fs::remove_file(truncated_path).unwrap();
}

#[test]
fn read_file_blocking_work_does_not_block_ping() {
    let path = write_temp_test_file("read_responsiveness", b"responsive\n");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    install_blocking_test_hook(path.clone(), started_tx, release_rx);

    let (tx, _rx) = std::sync::mpsc::channel();
    let (server_transport, client_transport) = tokio::io::duplex(8192);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let server_task = tokio::spawn(async move {
            run_mcp_server_loop(tx, Instant::now(), server_transport).await;
        });
        use rmcp::ServiceExt;
        let mut client = ().serve(client_transport).await.expect("serve client");
        let read_client = client.clone();
        let read_path = path.clone();
        let read_handle = tokio::spawn(async move {
            let mut params = rmcp::model::CallToolRequestParams::new("read_file");
            params.arguments = Some(
                rmcp::serde_json::json!({
                    "path": read_path.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 1
                })
                .as_object()
                .unwrap()
                .clone(),
            );
            read_client.call_tool(params).await
        });

        tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("read_file blocking work did not start");
        assert!(!read_handle.is_finished());

        let ping = client
            .call_tool(rmcp::model::CallToolRequestParams::new("ping"))
            .await
            .expect("ping should complete while file read is held");
        assert_eq!(only_text_content(&ping), "pong");
        assert!(!read_handle.is_finished());

        release_tx.send(()).unwrap();
        let read = read_handle
            .await
            .expect("read task")
            .expect("read call should complete");
        assert_eq!(
            read_file_structured_result(&read).status,
            ReadFileStatus::Completed
        );

        client.close().await.expect("close client");
        server_task.await.expect("server task");
    });

    std::fs::remove_file(path).unwrap();
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
        let started_marker = generate_temp_test_path("concurrency_started");
        let started_marker_for_launch = started_marker.clone();

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
            variables.insert(
                "RMCP_TEST_HELPER_STARTED_MARKER".to_string(),
                Some(started_marker_for_launch.to_string_lossy().into_owned()),
            );

            let mut call_params = rmcp::model::CallToolRequestParams::new("launch_process");
            call_params.arguments = Some(
                rmcp::serde_json::json!({
                    "process_name": make_helper_request().process_name,
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

        tokio::time::timeout(Duration::from_secs(3), async {
            while !started_marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("helper did not create its started marker");
        assert!(
            !launch_handle.is_finished(),
            "foreground launch completed before ping was sent"
        );

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
        let _ = std::fs::remove_file(&started_marker);

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
        let helper_args = vec!["arg1", "arg 2", "arg\"3"];
        req.arguments = Some(escape_windows_args(&helper_args));
    }
    #[cfg(not(target_os = "windows"))]
    {
        req.arguments = Some(vec![
            "arg1".to_string(),
            "arg 2".to_string(),
            "arg\"3".to_string(),
        ]);
    }

    let res = rt.block_on(async { server.execute_launch_process(req).await });
    assert!(matches!(res.status, LaunchProcessStatus::Completed));
    assert_eq!(res.stdout.unwrap().trim(), "arg1|arg 2|arg\"3");
}

#[derive(Clone, Copy)]
enum FakeTryWait {
    Exited,
    Running,
    Failed,
}

struct FakeChild {
    kill_succeeds: bool,
    wait_succeeds: bool,
    try_wait: FakeTryWait,
    calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl ChildOps for FakeChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.calls.lock().unwrap().push("kill");
        if self.kill_succeeds {
            Ok(())
        } else {
            Err(std::io::Error::other("injected kill failure"))
        }
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.calls.lock().unwrap().push("wait");
        if self.wait_succeeds {
            Ok(successful_exit_status())
        } else {
            Err(std::io::Error::other("injected wait failure"))
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.calls.lock().unwrap().push("try_wait");
        match self.try_wait {
            FakeTryWait::Exited => Ok(Some(successful_exit_status())),
            FakeTryWait::Running => Ok(None),
            FakeTryWait::Failed => Err(std::io::Error::other("injected status failure")),
        }
    }
}

#[cfg(target_os = "windows")]
fn successful_exit_status() -> std::process::ExitStatus {
    std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "exit /b 0"])
        .status()
        .unwrap()
}

#[cfg(not(target_os = "windows"))]
fn successful_exit_status() -> std::process::ExitStatus {
    std::process::Command::new("sh")
        .args(["-c", "true"])
        .status()
        .unwrap()
}

fn fake_child(
    kill_succeeds: bool,
    wait_succeeds: bool,
    try_wait: FakeTryWait,
) -> (
    FakeChild,
    std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        FakeChild {
            kill_succeeds,
            wait_succeeds,
            try_wait,
            calls: calls.clone(),
        },
        calls,
    )
}

#[test]
fn test_cleanup_uses_non_blocking_reaper_after_kill_failure() {
    let (child, calls) = fake_child(false, true, FakeTryWait::Running);
    let reaper_calls = calls.clone();
    let (status, error, _, _, _, outcome) = perform_cleanup(
        child,
        1,
        "Process timed out",
        true,
        "unused-stdout",
        "unused-stderr",
        move |_child| {
            reaper_calls.lock().unwrap().push("reaper");
            Ok(())
        },
    );

    assert_eq!(status, LaunchProcessStatus::StopFailed);
    assert_eq!(
        outcome,
        CleanupOutcome::KillFailedChildRunning {
            reaper_started: true
        }
    );
    assert_eq!(*calls.lock().unwrap(), ["kill", "try_wait", "reaper"]);
    let error = error.unwrap();
    assert!(error.contains("injected kill failure"));
    assert!(error.contains("may still be running"));
}

#[test]
fn test_cleanup_kill_failure_with_exited_child_is_reaped() {
    let stdout_path = generate_temp_test_path("cleanup_exited_stdout");
    let stderr_path = generate_temp_test_path("cleanup_exited_stderr");
    std::fs::write(&stdout_path, "recovered stdout").unwrap();
    std::fs::write(&stderr_path, "recovered stderr").unwrap();
    let (child, calls) = fake_child(false, true, FakeTryWait::Exited);
    let (status, error, exit_code, stdout, stderr, outcome) = perform_cleanup(
        child,
        1,
        "Status check failed",
        false,
        stdout_path.to_str().unwrap(),
        stderr_path.to_str().unwrap(),
        |_child| panic!("reaper should not start for an exited child"),
    );

    assert_eq!(status, LaunchProcessStatus::Completed);
    assert_eq!(outcome, CleanupOutcome::KillFailedChildExited);
    assert_eq!(exit_code, Some(0));
    assert_eq!(stdout.as_deref(), Some("recovered stdout"));
    assert_eq!(stderr.as_deref(), Some("recovered stderr"));
    assert_eq!(*calls.lock().unwrap(), ["kill", "try_wait"]);
    let error = error.unwrap();
    assert!(error.contains("successfully reaped"));
    assert!(!error.contains("may still be running"));
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
}

#[test]
fn test_timeout_cleanup_kill_failure_with_exited_child_is_completed() {
    let stdout_path = generate_temp_test_path("timeout_cleanup_exited_stdout");
    let stderr_path = generate_temp_test_path("timeout_cleanup_exited_stderr");
    std::fs::write(&stdout_path, "timeout stdout").unwrap();
    std::fs::write(&stderr_path, "timeout stderr").unwrap();
    let (child, calls) = fake_child(false, true, FakeTryWait::Exited);

    let (status, error, exit_code, stdout, stderr, outcome) = perform_cleanup(
        child,
        2,
        "Process timed out",
        true,
        stdout_path.to_str().unwrap(),
        stderr_path.to_str().unwrap(),
        |_child| panic!("reaper should not start for an exited child"),
    );

    assert_eq!(status, LaunchProcessStatus::Completed);
    assert_eq!(outcome, CleanupOutcome::KillFailedChildExited);
    assert_eq!(exit_code, Some(0));
    assert_eq!(stdout.as_deref(), Some("timeout stdout"));
    assert_eq!(stderr.as_deref(), Some("timeout stderr"));
    assert_eq!(*calls.lock().unwrap(), ["kill", "try_wait"]);
    assert!(!error.unwrap().contains("may still be running"));
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
}

#[test]
fn test_cleanup_success_returns_timeout_output() {
    let stdout_path = generate_temp_test_path("cleanup_stdout");
    let stderr_path = generate_temp_test_path("cleanup_stderr");
    std::fs::write(&stdout_path, "final stdout").unwrap();
    std::fs::write(&stderr_path, "final stderr").unwrap();
    let (child, calls) = fake_child(true, true, FakeTryWait::Running);

    let (status, error, exit_code, stdout, stderr, outcome) = perform_cleanup(
        child,
        1,
        "Process timed out",
        true,
        stdout_path.to_str().unwrap(),
        stderr_path.to_str().unwrap(),
        |_child| panic!("reaper should not start after successful cleanup"),
    );

    assert_eq!(status, LaunchProcessStatus::TimedOutStopped);
    assert_eq!(outcome, CleanupOutcome::KillSucceeded);
    assert_eq!(exit_code, Some(0));
    assert_eq!(stdout.as_deref(), Some("final stdout"));
    assert_eq!(stderr.as_deref(), Some("final stderr"));
    assert!(error.is_none());
    assert_eq!(*calls.lock().unwrap(), ["kill", "wait"]);
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
}

#[test]
fn test_cleanup_wait_failure_starts_reaper() {
    let (child, calls) = fake_child(true, false, FakeTryWait::Running);
    let reaper_calls = calls.clone();
    let (status, error, _, _, _, outcome) = perform_cleanup(
        child,
        1,
        "Status check failed",
        false,
        "unused-stdout",
        "unused-stderr",
        move |_child| {
            reaper_calls.lock().unwrap().push("reaper");
            Ok(())
        },
    );

    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert_eq!(outcome, CleanupOutcome::WaitFailedReaperStarted);
    assert_eq!(*calls.lock().unwrap(), ["kill", "wait", "reaper"]);
    assert!(error.unwrap().contains("injected wait failure"));
}

#[test]
fn test_cleanup_reaper_start_failure_is_cautious() {
    let (child, calls) = fake_child(false, true, FakeTryWait::Running);
    let (status, error, _, _, _, outcome) = perform_cleanup(
        child,
        1,
        "Process timed out",
        true,
        "unused-stdout",
        "unused-stderr",
        |_child| Err(std::io::Error::other("injected reaper failure")),
    );

    assert_eq!(status, LaunchProcessStatus::StopFailed);
    assert_eq!(
        outcome,
        CleanupOutcome::KillFailedChildRunning {
            reaper_started: false
        }
    );
    assert_eq!(*calls.lock().unwrap(), ["kill", "try_wait"]);
    let error = error.unwrap();
    assert!(error.contains("injected reaper failure"));
    assert!(error.contains("may still be running"));
    assert!(error.contains("may remain unreaped"));
}

#[test]
fn test_cleanup_unknown_status_starts_reaper_without_waiting() {
    let (child, calls) = fake_child(false, true, FakeTryWait::Failed);
    let reaper_calls = calls.clone();
    let (status, error, _, _, _, outcome) = perform_cleanup(
        child,
        1,
        "Status check failed",
        false,
        "unused-stdout",
        "unused-stderr",
        move |_child| {
            reaper_calls.lock().unwrap().push("reaper");
            Ok(())
        },
    );

    assert_eq!(status, LaunchProcessStatus::WaitFailed);
    assert_eq!(
        outcome,
        CleanupOutcome::KillFailedStatusUnknown {
            reaper_started: true
        }
    );
    assert_eq!(*calls.lock().unwrap(), ["kill", "try_wait", "reaper"]);
    let error = error.unwrap();
    assert!(error.contains("injected status failure"));
    assert!(error.contains("may still be running"));
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
