use crate::mcp::{McpServer, UiEventKind};
use rmcp::{schemars::JsonSchema, serde::Deserialize, serde::Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct PingResult {
    pub(crate) message: String,
}

impl McpServer {
    pub async fn ping_impl(&self) -> String {
        self.send_event(UiEventKind::PingRequested);
        let res = "pong".to_string();
        self.send_event(UiEventKind::PingResponded);
        res
    }
}
