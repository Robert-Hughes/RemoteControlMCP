use crate::mcp::McpServer;
use rmcp::{schemars::JsonSchema, serde::Deserialize, serde::Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct PingResult {
    pub(crate) message: String,
}

impl McpServer {
    pub async fn ping_impl(&self) -> String {
        "pong".to_string()
    }
}
