//! MCP server implementation.

use anyhow::{Context, Result, anyhow};
use std::io::{BufRead, Write};
use tracing::{debug, error, info, trace, warn};

use super::types::*;

/// Trait for handling MCP tool calls.
pub trait ToolHandler: Send + Sync {
    /// Returns the list of available tools.
    fn list_tools(&self) -> Vec<Tool>;

    /// Handles a tool call and returns the result.
    fn call_tool(&self, name: &str, arguments: Option<serde_json::Value>)
    -> Result<CallToolResult>;
}

/// MCP server that communicates over stdin/stdout.
pub struct McpServer<H: ToolHandler> {
    handler: H,
    initialized: bool,
}

impl<H: ToolHandler> McpServer<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            initialized: false,
        }
    }

    /// Runs the MCP server, reading from stdin and writing to stdout.
    pub fn run(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();

        info!("MCP server starting, waiting for requests on stdin");

        for line in stdin.lock().lines() {
            let line = line.context("Failed to read from stdin")?;

            if line.is_empty() {
                continue;
            }

            trace!("Received: {}", line);

            match self.handle_message(&line) {
                Ok(Some(response)) => {
                    let response_json = serde_json::to_string(&response)?;
                    trace!("Sending: {}", response_json);
                    writeln!(stdout, "{}", response_json)?;
                    stdout.flush()?;
                }
                Ok(None) => {
                    // Notification, no response needed
                }
                Err(e) => {
                    error!("Error handling message: {}", e);
                    // Try to send error response if we can parse the id
                    if let Ok(req) = serde_json::from_str::<Request>(&line) {
                        let response = Response::error(req.id, INTERNAL_ERROR, e.to_string());
                        let response_json = serde_json::to_string(&response)?;
                        writeln!(stdout, "{}", response_json)?;
                        stdout.flush()?;
                    }
                }
            }
        }

        info!("MCP server shutting down (stdin closed)");
        Ok(())
    }

    fn handle_message(&mut self, line: &str) -> Result<Option<Response>> {
        // Try to parse as request first
        if let Ok(request) = serde_json::from_str::<Request>(line) {
            let response = self.handle_request(request)?;
            return Ok(Some(response));
        }

        // Try to parse as notification
        if let Ok(notification) = serde_json::from_str::<Notification>(line) {
            self.handle_notification(notification)?;
            return Ok(None);
        }

        Err(anyhow!(
            "Failed to parse message as request or notification"
        ))
    }

    fn handle_request(&mut self, request: Request) -> Result<Response> {
        debug!("Handling request: {} (id={:?})", request.method, request.id);

        match request.method.as_str() {
            "initialize" => self.handle_initialize(request),
            "tools/list" => self.handle_tools_list(request),
            "tools/call" => self.handle_tools_call(request),
            "ping" => Ok(Response::success(request.id, serde_json::json!({}))),
            _ => {
                warn!("Unknown method: {}", request.method);
                Ok(Response::error(
                    request.id,
                    METHOD_NOT_FOUND,
                    format!("Unknown method: {}", request.method),
                ))
            }
        }
    }

    fn handle_notification(&mut self, notification: Notification) -> Result<()> {
        debug!("Handling notification: {}", notification.method);

        match notification.method.as_str() {
            "notifications/initialized" => {
                info!("MCP client initialized");
                self.initialized = true;
            }
            "notifications/cancelled" => {
                debug!("Request cancelled");
            }
            _ => {
                debug!("Ignoring unknown notification: {}", notification.method);
            }
        }

        Ok(())
    }

    fn handle_initialize(&mut self, request: Request) -> Result<Response> {
        let params: InitializeParams = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .context("Invalid initialize params")?
            .ok_or_else(|| anyhow!("Missing initialize params"))?;

        info!(
            "MCP client connecting: {} v{}",
            params.client_info.name,
            params.client_info.version.as_deref().unwrap_or("unknown")
        );
        info!("Protocol version: {}", params.protocol_version);

        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: None }),
            },
            server_info: ServerInfo {
                name: "catenary".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            },
        };

        Ok(Response::success(request.id, result))
    }

    fn handle_tools_list(&self, request: Request) -> Result<Response> {
        let tools = self.handler.list_tools();
        debug!("Listing {} tools", tools.len());

        let result = ListToolsResult { tools };
        Ok(Response::success(request.id, result))
    }

    fn handle_tools_call(&self, request: Request) -> Result<Response> {
        let params: CallToolParams = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .context("Invalid tools/call params")?
            .ok_or_else(|| anyhow!("Missing tools/call params"))?;

        debug!("Calling tool: {}", params.name);

        match self.handler.call_tool(&params.name, params.arguments) {
            Ok(result) => Ok(Response::success(request.id, result)),
            Err(e) => {
                error!("Tool call failed: {}", e);
                Ok(Response::success(
                    request.id,
                    CallToolResult::error(e.to_string()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler;

    impl ToolHandler for TestHandler {
        fn list_tools(&self) -> Vec<Tool> {
            vec![Tool {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            }]
        }

        fn call_tool(
            &self,
            name: &str,
            _arguments: Option<serde_json::Value>,
        ) -> Result<CallToolResult> {
            match name {
                "test_tool" => Ok(CallToolResult::text("Test result")),
                "error_tool" => Err(anyhow!("Test error")),
                _ => Err(anyhow!("Unknown tool: {}", name)),
            }
        }
    }

    #[test]
    fn test_handle_initialize() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "test-client",
                    "version": "1.0.0"
                }
            })),
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.result.is_some());
        assert!(response.error.is_none());

        let result: InitializeResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.server_info.name, "catenary");
    }

    #[test]
    fn test_handle_tools_list() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "tools/list".to_string(),
            params: None,
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.result.is_some());

        let result: ListToolsResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "test_tool");
    }

    #[test]
    fn test_handle_tools_call_success() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(3),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "test_tool",
                "arguments": {}
            })),
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.result.is_some());

        let result: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(result.is_error.is_none());
    }

    #[test]
    fn test_handle_tools_call_error() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(4),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "error_tool"
            })),
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.result.is_some());

        let result: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn test_handle_unknown_method() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(5),
            method: "unknown/method".to_string(),
            params: None,
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn test_handle_ping() {
        let mut server = McpServer::new(TestHandler);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(6),
            method: "ping".to_string(),
            params: None,
        };

        let response = server.handle_request(request).unwrap();
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }
}
