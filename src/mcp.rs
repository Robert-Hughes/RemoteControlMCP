use rmcp::{ServerHandler, handler::server::tool::ToolRouter, tool, tool_handler, tool_router};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub enum UiEventKind {
    WorkerStarted,
    ServerStarting,
    WaitingForClient,
    ClientConnected,
    PingRequested,
    PingResponded,
    ServerStopped,
    ServerError { error: String },
}

pub struct UiEvent {
    pub elapsed: Duration,
    pub kind: UiEventKind,
}

#[derive(Clone)]
pub struct McpServer {
    tx: Sender<UiEvent>,
    start_time: Instant,
    // The tool_router field is required by the #[tool_router] macro from the rmcp SDK,
    // but the compiler flags it as dead code because it is not read directly in our code.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
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
        description = "Check whether the local Remote Control MCP server is running and responding."
    )]
    async fn ping(&self) -> String {
        self.send_event(UiEventKind::PingRequested);
        let res = "pong".to_string();
        self.send_event(UiEventKind::PingResponded);
        res
    }
}

#[tool_handler(
    name = "remote-control-mcp",
    version = "0.1.0",
    instructions = "Remote Control MCP Server"
)]
impl ServerHandler for McpServer {
    async fn initialize(
        &self,
        _request: rmcp::model::InitializeRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, rmcp::ErrorData> {
        self.send_event(UiEventKind::ClientConnected);
        Ok(self.get_info())
    }
}

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

    rt.block_on(async move {
        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind: UiEventKind::WorkerStarted,
        });

        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind: UiEventKind::ServerStarting,
        });

        let service = McpServer::new(tx.clone(), start_time);

        use tokio::io::{stdin, stdout};
        let transport = (stdin(), stdout());

        use rmcp::ServiceExt;

        let server = match service.serve(transport).await {
            Ok(server) => server,
            Err(e) => {
                let _ = tx.send(UiEvent {
                    elapsed: start_time.elapsed(),
                    kind: UiEventKind::ServerError {
                        error: format!("Server serve failed: {}", e),
                    },
                });
                return;
            }
        };

        let _ = tx.send(UiEvent {
            elapsed: start_time.elapsed(),
            kind: UiEventKind::WaitingForClient,
        });

        match server.waiting().await {
            Ok(_) => {
                let _ = tx.send(UiEvent {
                    elapsed: start_time.elapsed(),
                    kind: UiEventKind::ServerStopped,
                });
            }
            Err(e) => {
                let _ = tx.send(UiEvent {
                    elapsed: start_time.elapsed(),
                    kind: UiEventKind::ServerError {
                        error: format!("Server error: {}", e),
                    },
                });
            }
        }
    });
}
