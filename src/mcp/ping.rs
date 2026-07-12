use crate::mcp::{McpServer, UiEventKind};

impl McpServer {
    pub async fn ping_impl(&self) -> String {
        self.send_event(UiEventKind::PingRequested);
        let res = "pong".to_string();
        self.send_event(UiEventKind::PingResponded);
        res
    }
}
