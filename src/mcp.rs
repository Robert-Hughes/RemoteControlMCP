use rmcp::{ServerHandler, handler::server::tool::ToolRouter, tool, tool_handler, tool_router};
use rmcp::{schemars, serde};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod file_path;
mod launch_process;
mod ping;
mod read_binary_file;
mod read_file;
mod write_file;

const GENERAL_INSTRUCTIONS: &str = include_str!("../instructions/GENERAL.md");
const LOCAL_INSTRUCTIONS_RELATIVE_PATH: &str = "instructions/LOCAL.md";
const MACHINE_INSTRUCTIONS_HEADING: &str = "# Machine-specific instructions";
// This intentionally stays short because the ChatGPT MCP connector has been observed silently
// truncating longer MCP initialisation instruction strings, preventing later machine-specific
// instructions from reaching the model.
const BOOTSTRAP_INSTRUCTIONS: &str = "Call the get_instructions tool once to get full instructions on how to use this MCP server; the instructions are stable, so there is no need to call it again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalInstructionsDiagnostic {
    Loaded { path: PathBuf },
    Warning { path: PathBuf, message: String },
}

struct LoadedServerInstructions {
    instructions: Arc<str>,
    diagnostic: LocalInstructionsDiagnostic,
}

fn local_instructions_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(LOCAL_INSTRUCTIONS_RELATIVE_PATH)
}

fn read_local_instructions(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn compose_instructions(local_instructions: Option<&str>) -> Arc<str> {
    let general = GENERAL_INSTRUCTIONS.trim();
    let Some(local) = local_instructions
        .map(str::trim)
        .filter(|contents| !contents.is_empty())
    else {
        return Arc::from(general);
    };

    Arc::from(format!(
        "{general}\n\n---\n\n{MACHINE_INSTRUCTIONS_HEADING}\n\n{local}"
    ))
}

fn warning_instructions(path: &Path, message: String) -> LoadedServerInstructions {
    eprintln!(
        "Warning: failed to load machine-specific MCP instructions from {}: {message}",
        path.display()
    );
    LoadedServerInstructions {
        instructions: compose_instructions(None),
        diagnostic: LocalInstructionsDiagnostic::Warning {
            path: path.to_path_buf(),
            message,
        },
    }
}

fn load_server_instructions_from_path(path: &Path) -> LoadedServerInstructions {
    match read_local_instructions(path) {
        Ok(Some(contents)) if !contents.trim().is_empty() => {
            eprintln!(
                "Loaded machine-specific MCP instructions from {}",
                path.display()
            );
            LoadedServerInstructions {
                instructions: compose_instructions(Some(&contents)),
                diagnostic: LocalInstructionsDiagnostic::Loaded {
                    path: path.to_path_buf(),
                },
            }
        }
        Ok(Some(_)) => warning_instructions(path, "file is empty".to_string()),
        Ok(None) => warning_instructions(path, "file not found".to_string()),
        Err(error) => warning_instructions(path, error.to_string()),
    }
}

fn load_server_instructions() -> LoadedServerInstructions {
    load_server_instructions_from_path(&local_instructions_path())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchProcessStatus {
    Completed,
    Detached,
    DetachedWithStopTimeout,
    TimedOutDetached,
    TimedOutStopped,
    SetupFailed,
    LaunchProcessFailed,
    WaitFailed,
    StopFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetInstructionsResult {
    pub instructions: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetInstructionsRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LaunchProcessResult {
    pub status: LaunchProcessStatus,
    pub error: Option<String>,
    #[schemars(with = "Option<ProcessIdSchema>")]
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub stdout_file: Option<String>,
    pub stderr_file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadFileStatus {
    Completed,
    Truncated,
    NotFound,
    AccessDenied,
    NotAFile,
    ReadFailed,
    LineTooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadBinaryFileStatus {
    Completed,
    NotFound,
    AccessDenied,
    NotAFile,
    TooLarge,
    ReadFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadBinaryFileContentKind {
    Image,
    EmbeddedResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteFileStatus {
    Completed,
    Created,
    NotFound,
    ParentNotFound,
    ParentNotADirectory,
    AccessDenied,
    NotAFile,
    RangeOutOfBounds,
    ReadFailed,
    WriteFailed,
    ReplaceFailed,
}

fn positive_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "integer", "minimum": 1 })
}

fn nullable_positive_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            { "type": "integer", "minimum": 1 },
            { "type": "null" }
        ]
    })
}

fn nonnegative_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "integer", "minimum": 0 })
}

fn nullable_nonnegative_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            { "type": "integer", "minimum": 0 },
            { "type": "null" }
        ]
    })
}

struct ProcessIdSchema;

impl JsonSchema for ProcessIdSchema {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProcessId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "minimum": 0,
            "maximum": 4_294_967_295_u64
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileRequest {
    pub path: String,
    #[schemars(schema_with = "positive_integer_schema")]
    pub start_line: u64,
    #[schemars(schema_with = "positive_integer_schema")]
    pub end_line: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileResult {
    pub status: ReadFileStatus,
    pub error: Option<String>,
    pub path: String,
    #[schemars(schema_with = "positive_integer_schema")]
    pub requested_start_line: u64,
    #[schemars(schema_with = "positive_integer_schema")]
    pub requested_end_line: u64,
    #[schemars(schema_with = "nullable_positive_integer_schema")]
    pub actual_start_line: Option<u64>,
    #[schemars(schema_with = "nullable_positive_integer_schema")]
    pub actual_end_line: Option<u64>,
    pub text: String,
    pub eof: Option<bool>,
    #[schemars(schema_with = "nullable_positive_integer_schema")]
    pub next_start_line: Option<u64>,
    pub lossy_utf8: bool,
}

fn binary_max_bytes_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "maximum": read_binary_file::MAX_BINARY_FILE_BYTES
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadBinaryFileRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "binary_max_bytes_schema")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadBinaryFileResult {
    pub status: ReadBinaryFileStatus,
    pub error: Option<String>,
    pub path: String,
    pub size: Option<u64>,
    pub mime_type: Option<String>,
    pub content_kind: Option<ReadBinaryFileContentKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteFileRequest {
    pub path: String,
    #[schemars(schema_with = "positive_integer_schema")]
    pub start_line: u64,
    #[schemars(schema_with = "positive_integer_schema")]
    pub end_line: u64,
    pub text: String,
    pub create_if_missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteFileResult {
    pub status: WriteFileStatus,
    pub error: Option<String>,
    pub path: String,
    #[schemars(schema_with = "positive_integer_schema")]
    pub requested_start_line: u64,
    #[schemars(schema_with = "positive_integer_schema")]
    pub requested_end_line: u64,
    #[schemars(schema_with = "nullable_nonnegative_integer_schema")]
    pub replaced_line_count: Option<u64>,
    #[schemars(schema_with = "nonnegative_integer_schema")]
    pub inserted_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvironmentConfig {
    #[serde(default = "default_inherit_environment")]
    #[schemars(default = "default_inherit_environment")]
    pub inherit: bool,
    pub variables: std::collections::HashMap<String, Option<String>>,
}

fn default_inherit_environment() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimeoutAction {
    Detach,
    Stop,
}

fn launch_process_max_output_bytes_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "description": "Maximum number of bytes returned inline for each of stdout and stderr. Defaults to 16384 bytes. If a stream exceeds this limit, its beginning is truncated from the inline result; the complete output remains available in the corresponding stdout_file or stderr_file."
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LaunchProcessRequest {
    pub working_directory: Option<String>,
    pub process_name: String,

    // Omitting `None` prevents Schemars advertising `default: null`, which MCP
    // Inspector would otherwise use to initialise the optional field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub arguments: Option<Vec<String>>,

    pub environment: EnvironmentConfig,
    pub detached: bool,
    pub timeout_ms: Option<u64>,
    pub timeout_action: Option<TimeoutAction>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "launch_process_max_output_bytes_schema")]
    pub max_output_bytes: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub(crate) u64);

impl RequestId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestData {
    Ping,
    GetInstructions,
    LaunchProcess {
        command_line: String,
        working_directory: Option<String>,
        detached: bool,
        timeout_ms: Option<u64>,
        timeout_action: Option<TimeoutAction>,
    },
    ReadFile {
        path: String,
        start_line: u64,
        end_line: u64,
    },
    ReadBinaryFile {
        path: String,
        max_bytes: Option<u64>,
    },
    WriteFile {
        path: String,
        start_line: u64,
        end_line: u64,
        replacement_bytes: u64,
        create_if_missing: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestUpdate {
    PingCompleted,
    GetInstructionsCompleted,
    LaunchProcessResponded {
        status: LaunchProcessStatus,
        error: Option<String>,
        pid: Option<u32>,
        exit_code: Option<i32>,
        stdout: Option<String>,
        stderr: Option<String>,
        stdout_file: Option<String>,
        stderr_file: Option<String>,
    },
    LaunchProcessOutputProgress {
        pid: u32,
        stdout_lines: u64,
        stderr_lines: u64,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    ReadFileResponded {
        status: ReadFileStatus,
        error: Option<String>,
        actual_start_line: Option<u64>,
        actual_end_line: Option<u64>,
        next_start_line: Option<u64>,
        eof: Option<bool>,
        text: String,
    },
    ReadBinaryFileResponded {
        status: ReadBinaryFileStatus,
        error: Option<String>,
        size: Option<u64>,
        mime_type: Option<String>,
        content_kind: Option<ReadBinaryFileContentKind>,
    },
    WriteFileResponded {
        status: WriteFileStatus,
        error: Option<String>,
        replaced_line_count: Option<u64>,
        inserted_bytes: u64,
    },
    Rejected {
        error: String,
    },
    InternalFailure {
        error: String,
    },
    LaunchProcessBackgroundError {
        pid: u32,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEventKind {
    WorkerStarted,
    ServerStarting,
    ServerListening {
        endpoint: String,
    },
    HttpConnectionOpened,
    HttpConnectionClosed,
    ClientConnected,
    ClientDisconnected,
    ClientInitialized,
    LocalInstructionsDiagnostic {
        diagnostic: LocalInstructionsDiagnostic,
    },
    RequestStarted {
        id: RequestId,
        request: RequestData,
        started_at: chrono::DateTime<chrono::Local>,
    },
    RequestUpdated {
        id: RequestId,
        update: RequestUpdate,
    },
    ServerStopped,
    ServerError {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiEvent {
    pub elapsed: Duration,
    pub kind: UiEventKind,
}

#[derive(Clone)]
pub struct McpServer {
    tx: Sender<UiEvent>,
    start_time: Instant,
    next_request_id: Arc<AtomicU64>,
    tool_router: ToolRouter<Self>,
    instructions: Arc<str>,
    connection_guard: Option<Arc<ConnectionGuard>>,
}

struct ConnectionGuard {
    tx: Sender<UiEvent>,
    start_time: Instant,
}

struct TrackedHttpIo<T> {
    inner: T,
    tx: Sender<UiEvent>,
    start_time: Instant,
}

impl<T> TrackedHttpIo<T> {
    fn new(inner: T, tx: Sender<UiEvent>, start_time: Instant) -> Self {
        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind: UiEventKind::HttpConnectionOpened,
        });
        Self {
            inner,
            tx,
            start_time,
        }
    }
}

impl<T> Drop for TrackedHttpIo<T> {
    fn drop(&mut self) {
        let _ = self.tx.send(UiEvent {
            elapsed: self.start_time.elapsed(),
            kind: UiEventKind::HttpConnectionClosed,
        });
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TrackedHttpIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TrackedHttpIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct TrackedHttpListener {
    inner: tokio::net::TcpListener,
    tx: Sender<UiEvent>,
    start_time: Instant,
}

impl axum::serve::Listener for TrackedHttpListener {
    type Io = TrackedHttpIo<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = axum::serve::Listener::accept(&mut self.inner).await;
        (
            TrackedHttpIo::new(stream, self.tx.clone(), self.start_time),
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(UiEvent {
            elapsed: self.start_time.elapsed(),
            kind: UiEventKind::ClientDisconnected,
        });
    }
}

#[cfg(test)]
pub mod test_hooks {
    use std::sync::Mutex;
    use std::sync::mpsc::Sender;

    static COMPLETION_SENDERS: Mutex<Vec<Sender<u32>>> = Mutex::new(Vec::new());

    pub fn register_completion_sender(tx: Sender<u32>) {
        COMPLETION_SENDERS.lock().unwrap().push(tx);
    }

    pub fn notify_completion(pid: u32) {
        let mut senders = COMPLETION_SENDERS.lock().unwrap();
        senders.retain(|tx| tx.send(pid).is_ok());
    }
}

/// Build the advertised input schema for a tool whose handler now receives raw
/// JSON arguments, so the model still sees the full typed parameter schema.
fn input_schema_for<T>(tool: &str) -> Arc<rmcp::model::JsonObject>
where
    T: schemars::JsonSchema + std::any::Any,
{
    rmcp::handler::server::common::schema_for_input::<T>()
        .unwrap_or_else(|error| panic!("Invalid input schema for {tool}: {error}"))
}

/// Build an `isError: true` tool result carrying only a text explanation.
/// Tool-level failures must be returned as tool results, not JSON-RPC errors,
/// so clients relay the message to the model instead of reporting a protocol failure.
fn argument_error_result(message: impl Into<String>) -> rmcp::model::CallToolResult {
    rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(message.into())])
}

/// Turn a serde argument deserialisation error into an actionable message, naming
/// the exact argument that was omitted so the model can correct the call.
fn missing_argument_message(error: &rmcp::serde_json::Error, required: &[&str]) -> String {
    let message = error.to_string();
    let missing = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split('`').next());
    match missing {
        Some(field) => format!(
            "Missing required argument '{field}'. Required arguments: {}.",
            required.join(", ")
        ),
        None => format!("Invalid arguments: {error}"),
    }
}

#[tool_router]
impl McpServer {
    #[cfg(test)]
    pub fn new(tx: Sender<UiEvent>, start_time: Instant) -> Self {
        let loaded = load_server_instructions();
        Self::new_with_instructions(tx, start_time, loaded.instructions)
    }

    fn new_with_instructions(
        tx: Sender<UiEvent>,
        start_time: Instant,
        instructions: Arc<str>,
    ) -> Self {
        Self {
            tx,
            start_time,
            next_request_id: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
            instructions,
            connection_guard: None,
        }
    }

    fn for_http_session(&self) -> Self {
        self.send_event(UiEventKind::ClientConnected);
        let mut service = self.clone();
        service.connection_guard = Some(Arc::new(ConnectionGuard {
            tx: self.tx.clone(),
            start_time: self.start_time,
        }));
        service
    }

    fn send_event(&self, kind: UiEventKind) {
        let event = UiEvent {
            elapsed: self.start_time.elapsed(),
            kind,
        };
        let _ = self.tx.send(event);
    }

    fn start_request(&self, request: RequestData) -> RequestId {
        let id = RequestId(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        self.send_event(UiEventKind::RequestStarted {
            id,
            request,
            started_at: chrono::Local::now(),
        });
        id
    }

    fn update_request(&self, id: RequestId, update: RequestUpdate) {
        self.send_event(UiEventKind::RequestUpdated { id, update });
    }

    fn structured_result_with_content<T: Serialize>(
        content: Vec<rmcp::model::ContentBlock>,
        value: &T,
        is_error: bool,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let structured_content = rmcp::serde_json::to_value(value).map_err(|error| {
            rmcp::ErrorData::internal_error(
                format!("Failed to serialise tool structured content: {error}"),
                None,
            )
        })?;
        let mut result = if is_error {
            rmcp::model::CallToolResult::error(content)
        } else {
            rmcp::model::CallToolResult::success(content)
        };
        result.structured_content = Some(structured_content);
        Ok(result)
    }

    fn structured_result<T: Serialize>(
        summary: String,
        value: &T,
        is_error: bool,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        Self::structured_result_with_content(
            vec![rmcp::model::ContentBlock::text(summary)],
            value,
            is_error,
        )
    }

    fn finish_structured_request_with_content<T: Serialize>(
        &self,
        id: RequestId,
        content: Vec<rmcp::model::ContentBlock>,
        value: &T,
        is_error: bool,
        update: RequestUpdate,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        match Self::structured_result_with_content(content, value, is_error) {
            Ok(result) => {
                self.update_request(id, update);
                Ok(result)
            }
            Err(error) => {
                self.update_request(
                    id,
                    RequestUpdate::InternalFailure {
                        error: error.message.to_string(),
                    },
                );
                Err(error)
            }
        }
    }

    fn finish_structured_request<T: Serialize>(
        &self,
        id: RequestId,
        summary: String,
        value: &T,
        is_error: bool,
        update: RequestUpdate,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        match Self::structured_result(summary, value, is_error) {
            Ok(result) => {
                self.update_request(id, update);
                Ok(result)
            }
            Err(error) => {
                self.update_request(
                    id,
                    RequestUpdate::InternalFailure {
                        error: error.message.to_string(),
                    },
                );
                Err(error)
            }
        }
    }

    #[tool(
        description = "Get the full instructions on how to use this MCP server. Call this tool before calling any other tools.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<GetInstructionsResult>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_instructions(
        &self,
        _params: rmcp::handler::server::wrapper::Parameters<GetInstructionsRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let id = self.start_request(RequestData::GetInstructions);
        let instructions = self.instructions.to_string();
        let result = GetInstructionsResult {
            instructions: instructions.clone(),
        };
        self.finish_structured_request(
            id,
            instructions,
            &result,
            false,
            RequestUpdate::GetInstructionsCompleted,
        )
    }

    #[tool(
        description = "Check whether the local Remote Control MCP server is running and responding.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ping::PingResult>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn ping(&self) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let id = self.start_request(RequestData::Ping);
        let message = self.ping_impl().await;
        let result = ping::PingResult {
            message: message.clone(),
        };
        self.finish_structured_request(id, message, &result, false, RequestUpdate::PingCompleted)
    }

    #[tool(
        description = "Launch a local process on the host machine with optional working directory, arguments, environment configuration, timeout, and detachment options.",
        input_schema = input_schema_for::<LaunchProcessRequest>("launch_process"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<LaunchProcessResult>(),
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn launch_process(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.launch_process_impl(params).await
    }

    #[tool(
        description = "Read a 1-based inclusive line range from a local regular file.",
        input_schema = input_schema_for::<ReadFileRequest>("read_file"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ReadFileResult>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_file(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.read_file_impl(params).await
    }

    #[tool(
        description = "Read a bounded binary file. Images are returned as native MCP image content; other files are returned as native embedded binary resources. The server hard limit is 100 MB.",
        input_schema = input_schema_for::<ReadBinaryFileRequest>("read_binary_file"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ReadBinaryFileResult>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_binary_file(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.read_binary_file_impl(params).await
    }

    #[tool(
        description = "Replace a strict 1-based inclusive line range in a local regular file, or explicitly create a missing file.",
        input_schema = input_schema_for::<WriteFileRequest>("write_file"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<WriteFileResult>(),
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write_file(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.write_file_impl(params).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::InitializeResult, rmcp::ErrorData>> + Send
    {
        let mut info = self.get_info();
        if rmcp::model::ProtocolVersion::KNOWN_VERSIONS.contains(&request.protocol_version) {
            info.protocol_version = request.protocol_version;
        }

        self.send_event(UiEventKind::ClientInitialized);

        std::future::ready(Ok(info))
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "remote-control-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(BOOTSTRAP_INSTRUCTIONS.to_string())
    }
}

fn build_mcp_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}

pub const MCP_HTTP_BIND_ADDRESS: &str = "127.0.0.1:61337";
pub const MCP_HTTP_ENDPOINT: &str = "http://127.0.0.1:61337/mcp";

type HttpMcpService = rmcp::transport::streamable_http_server::StreamableHttpService<
    McpServer,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
>;

fn build_http_mcp_service(
    tx: Sender<UiEvent>,
    start_time: Instant,
    instructions: Arc<str>,
) -> HttpMcpService {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let service = McpServer::new_with_instructions(tx.clone(), start_time, instructions);
    let service_factory = move || Ok(service.for_http_session());

    StreamableHttpService::new(
        service_factory,
        Default::default(),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    )
}

pub fn run_mcp_server(tx: Sender<UiEvent>, start_time: Instant) {
    let rt = match build_mcp_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = tx.send(UiEvent {
                elapsed: start_time.elapsed(),
                kind: UiEventKind::ServerError {
                    error: format!("Tokio runtime builder failed: {}", e),
                },
            });
            return;
        }
    };

    let _ = tx.send(UiEvent {
        elapsed: start_time.elapsed(),
        kind: UiEventKind::WorkerStarted,
    });

    rt.block_on(async move {
        run_http_mcp_server(tx, start_time).await;
    });
}

async fn run_http_mcp_server(tx: Sender<UiEvent>, start_time: Instant) {
    let send_event = |kind| {
        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind,
        });
    };

    send_event(UiEventKind::ServerStarting);

    let LoadedServerInstructions {
        instructions,
        diagnostic,
    } = load_server_instructions();
    send_event(UiEventKind::LocalInstructionsDiagnostic { diagnostic });

    let listener = match tokio::net::TcpListener::bind(MCP_HTTP_BIND_ADDRESS).await {
        Ok(listener) => listener,
        Err(error) => {
            send_event(UiEventKind::ServerError {
                error: format!(
                    "Could not bind the loopback MCP endpoint {MCP_HTTP_ENDPOINT}: {error}"
                ),
            });
            return;
        }
    };

    let listener = TrackedHttpListener {
        inner: listener,
        tx: tx.clone(),
        start_time,
    };
    let http_service = build_http_mcp_service(tx.clone(), start_time, instructions);
    let router = axum::Router::new().nest_service("/mcp", http_service);

    send_event(UiEventKind::ServerListening {
        endpoint: MCP_HTTP_ENDPOINT.to_string(),
    });

    match axum::serve(listener, router).await {
        Ok(()) => send_event(UiEventKind::ServerStopped),
        Err(error) => send_event(UiEventKind::ServerError {
            error: format!("HTTP MCP server error: {error}"),
        }),
    }
}

#[cfg(test)]
async fn run_mcp_server_loop<T, A>(tx: Sender<UiEvent>, start_time: Instant, transport: T)
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, std::io::Error, A> + Send + 'static,
    A: Send + 'static,
{
    let send_event = |kind| {
        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind,
        });
    };

    send_event(UiEventKind::ServerStarting);

    let LoadedServerInstructions {
        instructions,
        diagnostic,
    } = load_server_instructions();
    send_event(UiEventKind::LocalInstructionsDiagnostic { diagnostic });
    let service = McpServer::new_with_instructions(tx.clone(), start_time, instructions);

    use rmcp::ServiceExt;

    let server = match service.serve(transport).await {
        Ok(server) => server,
        Err(error) => {
            send_event(UiEventKind::ServerError {
                error: format!("Server initialization error: {error}"),
            });
            return;
        }
    };
    match server.waiting().await {
        Ok(_) => {
            send_event(UiEventKind::ServerStopped);
        }
        Err(e) => {
            send_event(UiEventKind::ServerError {
                error: format!("Server error: {}", e),
            });
        }
    }
}

#[cfg(test)]
mod tests;
