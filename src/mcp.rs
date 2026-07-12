use rmcp::{ServerHandler, handler::server::tool::ToolRouter, tool, tool_handler, tool_router};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
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
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn ping(&self) -> String {
        self.send_event(UiEventKind::PingRequested);
        let res = "pong".to_string();
        self.send_event(UiEventKind::PingResponded);
        res
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
mod tests {
    use super::*;

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
            assert_eq!(tools.len(), 1);
            let tool = &tools[0];
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
    }
}
