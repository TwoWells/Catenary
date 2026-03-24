// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! MCP server implementation.

use anyhow::{Context, Result, anyhow};
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, error, info, trace, warn};

use super::types::{
    CallToolParams, CallToolResult, INTERNAL_ERROR, InitializeParams, InitializeResult,
    ListToolsResult, METHOD_NOT_FOUND, Notification, Request, RequestId, Response, Root,
    RootsListResult, ServerCapabilities, ServerInfo, Tool, ToolsCapability,
};
use crate::session::MessageLog;

/// MCP protocol versions this server supports (newest first).
const SUPPORTED_MCP_VERSIONS: &[&str] = &["2025-11-25", "2024-11-05"];

/// Trait for handling MCP tool calls.
pub trait ToolHandler: Send + Sync {
    /// Returns the list of available tools.
    fn list_tools(&self) -> Vec<Tool>;

    /// Handles a tool call and returns the result.
    ///
    /// `parent_id` is the database ID of the incoming MCP message that
    /// triggered this call. Implementations forward it to
    /// [`ToolServer::execute`](super::super::bridge::ToolServer::execute)
    /// so LSP messages are correlated with the triggering MCP request
    /// in the monitor.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool call fails for reasons other than the tool itself reporting an error.
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<i64>,
    ) -> Result<CallToolResult>;
}

/// MCP server that communicates over stdin/stdout.
/// Callback invoked when MCP client info is received during initialize.
pub type ClientInfoCallback = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Callback invoked when MCP roots are received or updated.
pub type RootsChangedCallback = Box<dyn Fn(Vec<Root>) -> Result<()> + Send + Sync>;

/// An MCP server implementation.
#[allow(
    clippy::struct_excessive_bools,
    reason = "Bools track independent server state flags"
)]
pub struct McpServer<H: ToolHandler> {
    handler: H,
    initialized: bool,
    message_log: Arc<MessageLog>,
    /// Name of the connected MCP client (learned during initialize).
    client_name: String,
    on_client_info: Option<ClientInfoCallback>,
    /// Whether the client advertised any `roots` capability.
    client_has_roots: bool,
    /// Flag: should we send a `roots/list` request after this message?
    should_fetch_roots: bool,
    /// Guard: are we currently inside `fetch_roots`? Prevents recursion.
    fetching_roots: bool,
    /// Counter for outbound request IDs (server-initiated).
    next_outbound_id: i64,
    /// Callback invoked when roots change.
    on_roots_changed: Option<RootsChangedCallback>,
    /// Shared flag set by `HookServer` when a `PreToolUse` hook fires.
    refresh_roots: Arc<AtomicBool>,
    /// Database ID of the current incoming message, set per `dispatch_message`.
    /// Read by `handle_tools_call` to supply `parent_id` to the tool handler.
    current_entry_id: i64,
}

impl<H: ToolHandler> McpServer<H> {
    /// Creates a new `McpServer`.
    pub fn new(handler: H, message_log: Arc<MessageLog>) -> Self {
        Self {
            handler,
            initialized: false,
            message_log,
            client_name: "unknown".to_string(),
            on_client_info: None,
            client_has_roots: false,
            should_fetch_roots: false,
            fetching_roots: false,
            next_outbound_id: 0,
            on_roots_changed: None,
            refresh_roots: Arc::new(AtomicBool::new(false)),
            current_entry_id: 0,
        }
    }

    /// Set a callback to be invoked when client info is received.
    #[must_use]
    pub fn on_client_info(mut self, callback: ClientInfoCallback) -> Self {
        self.on_client_info = Some(callback);
        self
    }

    /// Set a callback to be invoked when MCP roots are received or updated.
    #[must_use]
    pub fn on_roots_changed(mut self, callback: RootsChangedCallback) -> Self {
        self.on_roots_changed = Some(callback);
        self
    }

    /// Set a shared flag that the hook server uses to request a `roots/list` fetch.
    #[must_use]
    pub fn with_refresh_roots(mut self, flag: Arc<AtomicBool>) -> Self {
        self.refresh_roots = flag;
        self
    }

    /// Runs the MCP server, reading from stdin and writing to stdout.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from stdin or writing to stdout fails.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "stdin/stdout locks must be held for the entire run loop"
    )]
    pub fn run(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();

        info!("MCP server starting, waiting for requests on stdin");

        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .context("Failed to read from stdin")?;
            if bytes_read == 0 {
                break; // EOF
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            trace!("Received: {}", trimmed);

            // Log incoming message
            let (entry_id, method) =
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    let method = json
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("response")
                        .to_string();
                    let id = self.message_log.log(
                        "mcp",
                        &method,
                        "catenary",
                        &self.client_name,
                        None,
                        None,
                        &json,
                    );
                    (id, method)
                } else {
                    (0, String::new())
                };

            self.current_entry_id = entry_id;
            self.dispatch_message(trimmed, &mut writer, entry_id, &method)?;

            // Check if the hook server requested a roots refresh
            if self.refresh_roots.swap(false, Ordering::Acquire) {
                self.should_fetch_roots = true;
            }

            // Check if we need to fetch roots
            if self.should_fetch_roots
                && let Err(e) = self.fetch_roots(&mut reader, &mut writer)
            {
                error!("Failed to fetch roots: {}", e);
            }
        }

        info!("MCP server shutting down (stdin closed)");
        Ok(())
    }

    /// Dispatches a single message line, writing any response to `writer`.
    fn dispatch_message(
        &mut self,
        line: &str,
        writer: &mut impl Write,
        entry_id: i64,
        method: &str,
    ) -> Result<()> {
        match self.handle_message(line) {
            Ok(Some(response)) => {
                self.write_response(&response, writer, Some(entry_id), method)?;
            }
            Ok(None) => {
                // Notification, no response needed
            }
            Err(e) => {
                error!("Error handling message: {}", e);
                // Try to send error response if we can parse the id
                if let Ok(req) = serde_json::from_str::<Request>(line) {
                    let response = Response::error(req.id, INTERNAL_ERROR, e.to_string());
                    self.write_response(&response, writer, Some(entry_id), method)?;
                }
            }
        }
        Ok(())
    }

    /// Serializes, broadcasts, and writes a response.
    fn write_response(
        &self,
        response: &Response,
        writer: &mut impl Write,
        request_id: Option<i64>,
        method: &str,
    ) -> Result<()> {
        let response_json =
            serde_json::to_string(response).context("Failed to serialize response")?;
        trace!("Sending: {}", response_json);

        if let Ok(json) = serde_json::to_value(response) {
            self.message_log.log(
                "mcp",
                method,
                "catenary",
                &self.client_name,
                request_id,
                request_id,
                &json,
            );
        }

        writeln!(writer, "{response_json}")?;
        writer.flush()?;
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
            self.handle_notification(&notification);
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
            "ping" => Ok(Response::success(request.id, serde_json::json!({}))?),
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

    fn handle_notification(&mut self, notification: &Notification) {
        debug!("Handling notification: {}", notification.method);

        match notification.method.as_str() {
            "notifications/initialized" => {
                info!("MCP client initialized");
                self.initialized = true;
                if self.client_has_roots {
                    self.should_fetch_roots = true;
                }
            }
            "notifications/roots/list_changed" => {
                info!("MCP client roots changed");
                // Always honor — the client explicitly told us roots changed,
                // regardless of what it advertised during initialization.
                self.should_fetch_roots = true;
            }
            "notifications/cancelled" => {
                debug!("Request cancelled");
            }
            _ => {
                debug!("Ignoring unknown notification: {}", notification.method);
            }
        }
    }

    fn handle_initialize(&mut self, request: Request) -> Result<Response> {
        let params: InitializeParams = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .context("Invalid initialize params")?
            .ok_or_else(|| anyhow!("Missing initialize params"))?;

        self.client_name.clone_from(&params.client_info.name);
        let client_name = &params.client_info.name;
        let client_version = params.client_info.version.as_deref().unwrap_or("unknown");

        info!("MCP client connecting: {} v{}", client_name, client_version);
        info!("Protocol version requested: {}", params.protocol_version);

        // Negotiate protocol version per MCP spec: echo the requested
        // version if we support it, otherwise respond with our latest.
        let negotiated_version =
            if SUPPORTED_MCP_VERSIONS.contains(&params.protocol_version.as_str()) {
                params.protocol_version.clone()
            } else {
                info!(
                    "Unsupported protocol version '{}', responding with {}",
                    params.protocol_version, SUPPORTED_MCP_VERSIONS[0]
                );
                SUPPORTED_MCP_VERSIONS[0].to_string()
            };

        // Store whether client supports roots
        self.client_has_roots = params.capabilities.roots.is_some();

        if self.client_has_roots {
            info!("Client supports roots capability");
        }

        // Notify callback of client info
        if let Some(ref callback) = self.on_client_info {
            callback(client_name, client_version);
        }

        let result = InitializeResult {
            protocol_version: negotiated_version,
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
            },
            server_info: ServerInfo {
                name: "catenary".to_string(),
                version: Some(env!("CATENARY_VERSION").to_string()),
            },
            instructions: Some(
                "Catenary provides LSP-backed code intelligence tools. \
                 Its search tools include all available LSP information \
                 and condense grep-equivalent results into a heatmap. \
                 Post-edit LSP diagnostics are provided automatically via \
                 the notify hook. When multiple edits target the same file \
                 in one response, only the final diagnostics per file are \
                 authoritative \u{2014} ignore intermediate results."
                    .to_string(),
            ),
        };

        Ok(Response::success(request.id, result)?)
    }

    fn handle_tools_list(&self, request: Request) -> Result<Response> {
        let tools = self.handler.list_tools();
        debug!("Listing {} tools", tools.len());

        let result = ListToolsResult { tools };
        Ok(Response::success(request.id, result)?)
    }

    fn handle_tools_call(&self, request: Request) -> Result<Response> {
        let params: CallToolParams = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .context("Invalid tools/call params")?
            .ok_or_else(|| anyhow!("Missing tools/call params"))?;

        debug!("Calling tool: {}", params.name);

        let parent_id = Some(self.current_entry_id);
        match self
            .handler
            .call_tool(&params.name, params.arguments, parent_id)
        {
            Ok(result) => Ok(Response::success(request.id, result)?),
            Err(e) => {
                error!("Tool call failed: {}", e);
                Ok(Response::success(
                    request.id,
                    CallToolResult::error(e.to_string()),
                )?)
            }
        }
    }

    /// Generates a unique request ID for server-initiated requests.
    fn next_id(&mut self) -> RequestId {
        let id = self.next_outbound_id;
        self.next_outbound_id += 1;
        RequestId::String(format!("catenary-{id}"))
    }

    /// Sends a `roots/list` request to the client and processes the response.
    ///
    /// Handles interleaved client requests/notifications while waiting for
    /// the response. Uses `fetching_roots` guard to prevent recursion if
    /// `roots/list_changed` arrives during the fetch.
    fn fetch_roots(&mut self, reader: &mut impl BufRead, writer: &mut impl Write) -> Result<()> {
        if self.fetching_roots {
            debug!("Already fetching roots, skipping");
            return Ok(());
        }
        self.fetching_roots = true;
        self.should_fetch_roots = false;

        let result = self.fetch_roots_inner(reader, writer);
        self.fetching_roots = false;
        result
    }

    /// Inner implementation of [`Self::fetch_roots`].
    fn fetch_roots_inner(
        &mut self,
        reader: &mut impl BufRead,
        writer: &mut impl Write,
    ) -> Result<()> {
        let request_id = self.next_id();
        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            method: "roots/list".to_string(),
            params: None,
        };

        let request_json =
            serde_json::to_string(&request).context("Failed to serialize roots/list request")?;
        trace!("Sending roots/list request: {}", request_json);

        // Log outbound request
        let outbound_entry_id = if let Ok(json) = serde_json::to_value(&request) {
            self.message_log.log(
                "mcp",
                "roots/list",
                "catenary",
                &self.client_name,
                None,
                None,
                &json,
            )
        } else {
            0
        };

        writeln!(writer, "{request_json}")?;
        writer.flush()?;

        // Read lines until we get the matching response.
        // Buffer interleaved requests (id + method) until roots are applied,
        // so they execute against the updated PathValidator.
        // Notifications are dispatched immediately.
        let mut buffered: Vec<(String, i64, String)> = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .context("Failed to read from stdin during roots/list")?;
            if bytes_read == 0 {
                return Err(anyhow!(
                    "stdin closed while waiting for roots/list response"
                ));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            trace!("Received (during roots/list wait): {}", trimmed);

            // Parse JSON once for disambiguation and logging
            let json: serde_json::Value = serde_json::from_str(trimmed)
                .context("Failed to parse JSON during roots/list wait")?;

            // Response: has `id` + no `method` + (`result` or `error`)
            let is_response = json.get("id").is_some()
                && json.get("method").is_none()
                && (json.get("result").is_some() || json.get("error").is_some());

            if is_response {
                let response: Response =
                    serde_json::from_value(json).context("Failed to parse roots/list response")?;
                if response.id == request_id {
                    // Log the response with request_id pointing to the outbound request
                    if let Ok(resp_json) = serde_json::to_value(&response) {
                        self.message_log.log(
                            "mcp",
                            "roots/list",
                            "catenary",
                            &self.client_name,
                            Some(outbound_entry_id),
                            None,
                            &resp_json,
                        );
                    }
                    let result = self.handle_roots_response(response);
                    // Replay buffered requests against the updated roots
                    for (msg, buf_entry_id, buf_method) in &buffered {
                        self.dispatch_message(msg, writer, *buf_entry_id, buf_method)?;
                    }
                    return result;
                }
                warn!(
                    "Received response with unexpected ID {:?} while waiting for roots/list",
                    response.id
                );
                continue;
            }

            // Non-response: log the incoming message, then buffer or dispatch.
            let method = json
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("response")
                .to_string();
            let entry_id = self.message_log.log(
                "mcp",
                &method,
                "catenary",
                &self.client_name,
                None,
                None,
                &json,
            );

            // Requests (id + method) are buffered until roots are applied.
            // Notifications dispatch immediately.
            if json.get("id").is_some() && json.get("method").is_some() {
                buffered.push((trimmed.to_string(), entry_id, method));
            } else {
                self.dispatch_message(trimmed, writer, entry_id, &method)?;
            }
        }
    }

    /// Processes the response to a `roots/list` request.
    fn handle_roots_response(&self, response: Response) -> Result<()> {
        if let Some(error) = response.error {
            warn!(
                "roots/list request failed: {} (code {})",
                error.message, error.code
            );
            return Ok(()); // Non-fatal
        }

        let result_value = response
            .result
            .ok_or_else(|| anyhow!("roots/list response has neither result nor error"))?;

        let roots_result: RootsListResult =
            serde_json::from_value(result_value).context("Failed to parse roots/list result")?;

        info!(
            "Received {} root(s) from MCP client",
            roots_result.roots.len()
        );
        for root in &roots_result.roots {
            info!(
                "  Root: {} ({})",
                root.uri,
                root.name.as_deref().unwrap_or("unnamed")
            );
        }

        if let Some(ref callback) = self.on_roots_changed
            && let Err(e) = callback(roots_result.roots)
        {
            error!("Failed to apply roots: {}", e);
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
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
                annotations: None,
            }]
        }

        fn call_tool(
            &self,
            name: &str,
            _arguments: Option<serde_json::Value>,
            _parent_id: Option<i64>,
        ) -> Result<CallToolResult> {
            match name {
                "test_tool" => Ok(CallToolResult::text("Test result")),
                "error_tool" => Err(anyhow!("Test error")),
                _ => Err(anyhow!("Unknown tool: {name}")),
            }
        }
    }

    #[test]
    fn test_handle_initialize() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

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

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());
        assert!(response.error.is_none());

        let result: InitializeResult =
            serde_json::from_value(response.result.expect("response result"))?;
        assert_eq!(result.server_info.name, "catenary");
        assert_eq!(result.protocol_version, "2024-11-05");
        assert!(result.instructions.is_some());
        Ok(())
    }

    #[test]
    fn test_handle_tools_list() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "tools/list".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());

        let result: ListToolsResult =
            serde_json::from_value(response.result.expect("response result"))?;
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "test_tool");
        Ok(())
    }

    #[test]
    fn test_handle_tools_call_success() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(3),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "test_tool",
                "arguments": {}
            })),
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());

        let result: CallToolResult =
            serde_json::from_value(response.result.expect("response result"))?;
        assert!(result.is_error.is_none());
        Ok(())
    }

    #[test]
    fn test_handle_tools_call_error() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(4),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "error_tool"
            })),
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());

        let result: CallToolResult =
            serde_json::from_value(response.result.expect("response result"))?;
        assert_eq!(result.is_error, Some(true));
        Ok(())
    }

    #[test]
    fn test_handle_unknown_method() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(5),
            method: "unknown/method".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.error.is_some());
        assert_eq!(
            response.error.expect("response error").code,
            METHOD_NOT_FOUND
        );
        Ok(())
    }

    #[test]
    fn test_handle_ping() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(6),
            method: "ping".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        Ok(())
    }

    fn initialize_server(server: &mut McpServer<TestHandler>, with_roots: bool) -> Result<()> {
        let caps = if with_roots {
            serde_json::json!({"roots": {"listChanged": true}})
        } else {
            serde_json::json!({})
        };

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(99),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": caps,
                "clientInfo": {"name": "test", "version": "1.0"}
            })),
        };
        let _ = server.handle_request(request)?;
        Ok(())
    }

    #[test]
    fn test_roots_capability_stored_when_present() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        assert!(!server.client_has_roots);

        initialize_server(&mut server, true)?;
        assert!(server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_roots_capability_absent_by_default() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, false)?;
        assert!(!server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_should_fetch_roots_after_initialized() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        assert!(server.initialized);
        Ok(())
    }

    #[test]
    fn test_should_fetch_roots_on_list_changed() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/roots/list_changed".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        Ok(())
    }

    #[test]
    fn test_no_fetch_without_capability() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, false)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(!server.should_fetch_roots);
        Ok(())
    }

    #[test]
    fn test_fetch_roots_parses_response() -> Result<()> {
        use std::io::Cursor;
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        server.should_fetch_roots = true;

        // Mock stdin: the response to our roots/list request
        let response_json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {
                "roots": [
                    {"uri": "file:///tmp/project_a", "name": "Project A"},
                    {"uri": "file:///tmp/project_b"}
                ]
            }
        });
        let input = format!("{}\n", serde_json::to_string(&response_json)?);
        let mut reader = Cursor::new(input.into_bytes());
        let mut writer: Vec<u8> = Vec::new();

        server.fetch_roots(&mut reader, &mut writer)?;

        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].uri, "file:///tmp/project_a");
        assert_eq!(roots[0].name.as_deref(), Some("Project A"));
        assert_eq!(roots[1].uri, "file:///tmp/project_b");
        assert!(roots[1].name.is_none());
        drop(roots);

        // Verify the outbound request was written
        let output = String::from_utf8(writer)?;
        assert!(output.contains("roots/list"));
        assert!(output.contains("catenary-0"));
        Ok(())
    }

    #[test]
    fn test_fetch_roots_buffers_interleaved_request() -> Result<()> {
        use std::io::Cursor;
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        server.should_fetch_roots = true;

        // Mock stdin: a ping request arrives BEFORE the roots/list response.
        // The request should be buffered and replayed after roots are applied.
        let ping_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "ping"
        });
        let roots_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {"roots": [{"uri": "file:///tmp/test"}]}
        });
        let input = format!(
            "{}\n{}\n",
            serde_json::to_string(&ping_request)?,
            serde_json::to_string(&roots_response)?
        );
        let mut reader = Cursor::new(input.into_bytes());
        let mut writer: Vec<u8> = Vec::new();

        server.fetch_roots(&mut reader, &mut writer)?;

        // Verify roots were received
        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].uri, "file:///tmp/test");
        drop(roots);

        // Verify both the roots/list request AND the ping response were written,
        // and that the ping response (buffered) appears after the roots/list request.
        let output = String::from_utf8(writer)?;
        let roots_pos = output
            .find("roots/list")
            .ok_or_else(|| anyhow!("roots/list request not found in output"))?;
        let ping_pos = output
            .find(r#""id":42"#)
            .ok_or_else(|| anyhow!("ping response not found in output"))?;
        assert!(
            roots_pos < ping_pos,
            "ping response should appear after roots/list request (buffered)"
        );
        Ok(())
    }

    #[test]
    fn test_fetch_roots_handles_error_response() -> Result<()> {
        use std::io::Cursor;

        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;
        server.should_fetch_roots = true;

        // Mock stdin: an error response
        let error_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "error": {"code": -32601, "message": "roots/list not supported"}
        });
        let input = format!("{}\n", serde_json::to_string(&error_response)?);
        let mut reader = Cursor::new(input.into_bytes());
        let mut writer: Vec<u8> = Vec::new();

        // Should not error — error responses are non-fatal
        server.fetch_roots(&mut reader, &mut writer)?;
        assert!(!server.fetching_roots);
        Ok(())
    }

    #[test]
    fn test_list_changed_honored_without_capability() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        // Initialize WITHOUT roots capability
        initialize_server(&mut server, false)?;
        assert!(!server.client_has_roots);

        // Client sends roots/list_changed anyway — we must honor it
        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/roots/list_changed".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        Ok(())
    }

    #[test]
    fn test_roots_capability_without_list_changed() -> Result<()> {
        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));

        // Initialize with `roots: {}` (no listChanged field)
        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(99),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"roots": {}},
                "clientInfo": {"name": "test", "version": "1.0"}
            })),
        };
        let _ = server.handle_request(request)?;

        // roots.is_some() should be true even without listChanged
        assert!(server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_fetching_roots_reset_on_error() -> Result<()> {
        use std::io::Cursor;

        let mut server = McpServer::new(TestHandler, Arc::new(MessageLog::noop()));
        initialize_server(&mut server, true)?;
        server.should_fetch_roots = true;

        // Empty stdin — will cause EOF error during fetch
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();

        let result = server.fetch_roots(&mut reader, &mut writer);
        assert!(result.is_err());
        // fetching_roots must be reset even on error
        assert!(!server.fetching_roots);
        Ok(())
    }

    // ── MessageLog integration tests ─────────────────────────────────

    /// Row from the messages table for test assertions.
    struct MsgRow {
        method: String,
        client: String,
        request_id: Option<i64>,
    }

    /// Create a test DB and return a `MessageLog` backed by it.
    fn test_message_log() -> (Arc<MessageLog>, Arc<std::sync::Mutex<rusqlite::Connection>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = crate::db::open_and_migrate_at(&dir.keep().join("catenary").join("catenary.db"))
            .expect("open test db");
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
             VALUES ('test', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert session");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let log = Arc::new(MessageLog::new(conn.clone(), "test".to_string()));
        (log, conn)
    }

    /// Query all messages from the test DB, ordered by id.
    fn query_messages(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
        let c = conn.lock().expect("lock");
        let rows: Vec<MsgRow> = c
            .prepare("SELECT method, client, request_id FROM messages ORDER BY id")
            .expect("prepare")
            .query_map([], |row| {
                Ok(MsgRow {
                    method: row.get(0)?,
                    client: row.get(1)?,
                    request_id: row.get(2)?,
                })
            })
            .expect("query")
            .filter_map(std::result::Result::ok)
            .collect();
        drop(c);
        rows
    }

    #[test]
    fn test_mcp_log_initialize() -> Result<()> {
        let (log, conn) = test_message_log();
        let mut server = McpServer::new(TestHandler, log);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0.0"}
            })),
        };

        let line = serde_json::to_string(&request)?;
        let mut writer: Vec<u8> = Vec::new();

        // Parse and log the incoming message (simulating run loop)
        let json: serde_json::Value = serde_json::from_str(&line)?;
        let method = json
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("response")
            .to_string();
        let entry_id = server.message_log.log(
            "mcp",
            &method,
            "catenary",
            &server.client_name,
            None,
            None,
            &json,
        );
        server.dispatch_message(&line, &mut writer, entry_id, &method)?;

        let msgs = query_messages(&conn);
        assert_eq!(msgs.len(), 2, "should have request + response");
        assert_eq!(msgs[0].method, "initialize");
        assert!(msgs[0].request_id.is_none());
        assert_eq!(msgs[1].method, "initialize");
        assert_eq!(
            msgs[1].request_id,
            Some(entry_id),
            "response request_id should point to the incoming request"
        );
        Ok(())
    }

    #[test]
    fn test_mcp_log_tools_call() -> Result<()> {
        let (log, conn) = test_message_log();
        let mut server = McpServer::new(TestHandler, log);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "test_tool",
                "arguments": {}
            })),
        };

        let line = serde_json::to_string(&request)?;
        let mut writer: Vec<u8> = Vec::new();

        let json: serde_json::Value = serde_json::from_str(&line)?;
        let method = "tools/call".to_string();
        let entry_id = server.message_log.log(
            "mcp",
            &method,
            "catenary",
            &server.client_name,
            None,
            None,
            &json,
        );
        server.dispatch_message(&line, &mut writer, entry_id, &method)?;

        let msgs = query_messages(&conn);
        assert_eq!(msgs.len(), 2, "should have request + response");
        assert_eq!(msgs[0].method, "tools/call");
        assert_eq!(msgs[1].method, "tools/call");
        assert_eq!(
            msgs[1].request_id,
            Some(entry_id),
            "response request_id should point to the incoming request"
        );
        Ok(())
    }

    #[test]
    fn test_mcp_log_notification() -> Result<()> {
        let (log, conn) = test_message_log();
        let mut server = McpServer::new(TestHandler, log);

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };

        let line = serde_json::to_string(&notification)?;
        let mut writer: Vec<u8> = Vec::new();

        let json: serde_json::Value = serde_json::from_str(&line)?;
        let method = "notifications/initialized".to_string();
        let entry_id = server.message_log.log(
            "mcp",
            &method,
            "catenary",
            &server.client_name,
            None,
            None,
            &json,
        );
        server.dispatch_message(&line, &mut writer, entry_id, &method)?;

        let msgs = query_messages(&conn);
        assert_eq!(msgs.len(), 1, "notification has no response");
        assert_eq!(msgs[0].method, "notifications/initialized");
        assert!(msgs[0].request_id.is_none());
        Ok(())
    }

    #[test]
    fn test_mcp_log_client_name() -> Result<()> {
        let (log, conn) = test_message_log();
        let mut server = McpServer::new(TestHandler, log);

        // Initialize to set client_name
        let init_request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "2.0.0"}
            })),
        };

        let line = serde_json::to_string(&init_request)?;
        let mut writer: Vec<u8> = Vec::new();
        let json: serde_json::Value = serde_json::from_str(&line)?;
        let entry_id = server.message_log.log(
            "mcp",
            "initialize",
            "catenary",
            &server.client_name,
            None,
            None,
            &json,
        );
        server.dispatch_message(&line, &mut writer, entry_id, "initialize")?;

        // Now send a second request — client_name should be "claude-code"
        let ping = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "ping".to_string(),
            params: None,
        };

        let line = serde_json::to_string(&ping)?;
        let json: serde_json::Value = serde_json::from_str(&line)?;
        let entry_id = server.message_log.log(
            "mcp",
            "ping",
            "catenary",
            &server.client_name,
            None,
            None,
            &json,
        );
        server.dispatch_message(&line, &mut writer, entry_id, "ping")?;

        let msgs = query_messages(&conn);
        // Messages: init req, init resp, ping req, ping resp = 4
        assert_eq!(msgs.len(), 4);
        // The ping request (3rd message) should have client = "claude-code"
        assert_eq!(msgs[2].client, "claude-code");
        // The ping response (4th message) should also have client = "claude-code"
        assert_eq!(msgs[3].client, "claude-code");
        Ok(())
    }
}
