use rmcp::{ServerHandler, handler::server::tool::ToolRouter, tool, tool_handler, tool_router};
use rmcp::{schemars, serde};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

mod launch_process;
mod ping;
mod read_file;

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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LaunchProcessRequest {
    pub working_directory: Option<String>,
    pub process_name: String,

    #[cfg(target_os = "windows")]
    // Omitting `None` prevents Schemars advertising `default: null`, which MCP
    // Inspector would render as the literal text `null` in its string control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub arguments: Option<String>,

    #[cfg(not(target_os = "windows"))]
    // Omitting `None` prevents Schemars advertising `default: null`, which MCP
    // Inspector would otherwise use to initialise the optional field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub arguments: Option<Vec<String>>,

    pub environment: EnvironmentConfig,
    pub detached: bool,
    pub timeout_ms: Option<u64>,
    pub timeout_action: Option<TimeoutAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub(crate) u64);

impl RequestId {
    #[cfg(test)]
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestData {
    Ping,
    LaunchProcess {
        command_line: String,
    },
    ReadFile {
        path: String,
        start_line: u64,
        end_line: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestUpdate {
    PingCompleted,
    LaunchProcessResponded {
        status: LaunchProcessStatus,
        error: Option<String>,
        pid: Option<u32>,
        exit_code: Option<i32>,
    },
    ReadFileResponded {
        status: ReadFileStatus,
        error: Option<String>,
        actual_start_line: Option<u64>,
        actual_end_line: Option<u64>,
        next_start_line: Option<u64>,
        eof: Option<bool>,
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
    WaitingForClient,
    ClientConnected,
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

#[tool_router]
impl McpServer {
    pub fn new(tx: Sender<UiEvent>, start_time: Instant) -> Self {
        Self {
            tx,
            start_time,
            next_request_id: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
        }
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

    fn structured_success<T: Serialize>(
        summary: String,
        value: &T,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let structured_content = rmcp::serde_json::to_value(value).map_err(|error| {
            rmcp::ErrorData::internal_error(
                format!("Failed to serialise tool structured content: {error}"),
                None,
            )
        })?;
        let mut result =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(summary)]);
        result.structured_content = Some(structured_content);
        Ok(result)
    }

    fn finish_structured_request<T: Serialize>(
        &self,
        id: RequestId,
        summary: String,
        value: &T,
        update: RequestUpdate,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        match Self::structured_success(summary, value) {
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
        description = "Check whether the local Remote Control MCP server is running and responding.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ping::PingResult>()
            .expect("PingResult should generate a valid output schema"),
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
        self.finish_structured_request(id, message, &result, RequestUpdate::PingCompleted)
    }

    #[tool(
        description = "Launch a local process on the host machine with optional working directory, arguments, environment configuration, timeout, and detachment options.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<LaunchProcessResult>()
            .expect("LaunchProcessResult should generate a valid output schema"),
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn launch_process(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<LaunchProcessRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.launch_process_impl(params).await
    }

    #[tool(
        description = "Read a 1-based inclusive line range from a local regular file.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ReadFileResult>()
            .expect("ReadFileResult should generate a valid output schema"),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_file(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<ReadFileRequest>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.read_file_impl(params).await
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "remote-control-mcp",
    version = "0.1.0",
    instructions = "Remote Control MCP Server"
)]
impl ServerHandler for McpServer {}

fn build_mcp_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
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

    use tokio::io::{stdin, stdout};
    let transport = (stdin(), stdout());

    rt.block_on(async move {
        run_mcp_server_loop(tx, start_time, transport).await;
    });
}

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

    let service = McpServer::new(tx.clone(), start_time);

    send_event(UiEventKind::WaitingForClient);

    use rmcp::ServiceExt;

    match service.serve(transport).await {
        Ok(server) => {
            send_event(UiEventKind::ClientConnected);
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
        Err(e) => {
            send_event(UiEventKind::ServerError {
                error: format!("Server serve failed: {}", e),
            });
        }
    }
}

#[cfg(test)]
mod tests;
