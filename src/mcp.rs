use rmcp::{ServerHandler, handler::server::tool::ToolRouter, tool, tool_handler, tool_router};
use rmcp::{schemars, serde};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

mod launch_process;
mod ping;

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
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub stdout_file: Option<String>,
    pub stderr_file: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEventKind {
    WorkerStarted,
    ServerStarting,
    WaitingForClient,
    ClientConnected,
    PingRequested,
    PingResponded,
    LaunchProcessRequested {
        process_name: String,
    },
    LaunchProcessResponded {
        status: LaunchProcessStatus,
        pid: Option<u32>,
    },
    LaunchProcessRejected {
        error: String,
    },
    LaunchProcessBackgroundError {
        pid: u32,
        error: String,
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
        let message = self.ping_impl().await;
        let structured_content = rmcp::serde_json::to_value(ping::PingResult {
            message: message.clone(),
        })
        .map_err(|error| {
            rmcp::ErrorData::internal_error(
                format!("Failed to serialise ping structured content: {error}"),
                None,
            )
        })?;

        let mut result =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(message)]);
        result.structured_content = Some(structured_content);
        Ok(result)
    }

    #[tool(
        description = "Launch a local process on the host machine with optional working directory, arguments, environment configuration, timeout, and detachment options.",
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
    ) -> Result<rmcp::handler::server::wrapper::Json<LaunchProcessResult>, rmcp::ErrorData> {
        self.launch_process_impl(params).await
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "remote-control-mcp",
    version = "0.1.0",
    instructions = "Remote Control MCP Server"
)]
impl ServerHandler for McpServer {}

pub fn run_mcp_server(tx: Sender<UiEvent>, start_time: Instant) {
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
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
