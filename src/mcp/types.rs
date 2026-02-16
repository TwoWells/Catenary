/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! MCP (Model Context Protocol) type definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request (client-to-server or server-to-client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(
    dead_code,
    reason = "Fields required by JSON-RPC protocol but not all are read"
)]
pub struct Request {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID.
    pub id: RequestId,
    /// The method name.
    pub method: String,
    /// The request parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC notification (incoming from client or outgoing from server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(
    dead_code,
    reason = "Fields required by JSON-RPC protocol but not all are read"
)]
pub struct Notification {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The method name.
    pub method: String,
    /// The notification parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Request ID can be string or number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric ID.
    Number(i64),
    /// A string ID.
    String(String),
}

/// JSON-RPC response (client-to-server or server-to-client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID.
    pub id: RequestId,
    /// The result of the request, if successful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error, if the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    /// Creates a successful response.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the result cannot be converted to JSON.
    pub fn success(id: RequestId, result: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    /// Creates an error response.
    pub fn error(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(ResponseError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// JSON-RPC response error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// The error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The method was not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// An internal error occurred.
pub const INTERNAL_ERROR: i64 = -32603;

/// MCP initialize request params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    dead_code,
    reason = "Fields required by MCP protocol but not all are read"
)]
pub struct InitializeParams {
    /// The protocol version requested by the client.
    pub protocol_version: String,
    /// The capabilities of the client.
    pub capabilities: ClientCapabilities,
    /// Information about the client.
    pub client_info: ClientInfo,
}

/// MCP client capabilities.
#[derive(Debug, Clone, Deserialize)]
#[allow(
    dead_code,
    reason = "Fields required by MCP protocol but not all are read"
)]
pub struct ClientCapabilities {
    /// Roots-related capabilities.
    #[serde(default)]
    pub roots: Option<RootsCapability>,
    /// Sampling-related capabilities.
    #[serde(default)]
    pub sampling: Option<Value>,
}

/// Roots-related capabilities.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootsCapability {
    /// Whether the client supports listing changed roots.
    #[serde(default)]
    pub list_changed: bool,
}

/// Information about the MCP client.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientInfo {
    /// The name of the client.
    pub name: String,
    /// The version of the client.
    #[serde(default)]
    pub version: Option<String>,
}

/// MCP initialize response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// The protocol version supported by the server.
    pub protocol_version: String,
    /// The capabilities of the server.
    pub capabilities: ServerCapabilities,
    /// Information about the server.
    pub server_info: ServerInfo,
    /// Optional instructions for the client on how to use this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// MCP server capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Tools-related capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

/// Tools-related capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server supports listing changed tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Information about the MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// The name of the server.
    pub name: String,
    /// The version of the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Tool definition for tools/list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// The unique name of the tool.
    pub name: String,
    /// A human-readable description of the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON schema for the tool's input.
    pub input_schema: Value,
}

/// tools/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    /// The list of available tools.
    pub tools: Vec<Tool>,
}

/// tools/call request params.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolParams {
    /// The name of the tool to call.
    pub name: String,
    /// The arguments for the tool call.
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// tools/call response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// The content returned from the tool call.
    pub content: Vec<ToolContent>,
    /// Whether the tool call resulted in an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Content returned from a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolContent {
    /// Text content.
    Text {
        /// The text content.
        text: String,
    },
}

/// A root provided by the MCP client via `roots/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    /// The root URI. Must be a `file://` URI.
    pub uri: String,
    /// Optional human-readable name for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Response to a `roots/list` request.
#[derive(Debug, Clone, Deserialize)]
pub struct RootsListResult {
    /// The list of roots.
    pub roots: Vec<Root>,
}

impl CallToolResult {
    /// Creates a successful tool result with text content.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            is_error: None,
        }
    }

    /// Creates an error tool result with an error message.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            is_error: Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn test_deserialize_initialize_params() -> Result<()> {
        let json = r#"{
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "roots": { "listChanged": true }
            },
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }"#;

        let params: InitializeParams = serde_json::from_str(json)?;
        assert_eq!(params.protocol_version, "2024-11-05");
        assert_eq!(params.client_info.name, "test-client");
        Ok(())
    }

    #[test]
    fn test_serialize_initialize_result() -> Result<()> {
        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: None }),
            },
            server_info: ServerInfo {
                name: "catenary".to_string(),
                version: Some("0.1.0".to_string()),
            },
            instructions: None,
        };

        let json = serde_json::to_string(&result)?;
        assert!(json.contains("protocolVersion"));
        assert!(json.contains("catenary"));
        Ok(())
    }

    #[test]
    fn test_serialize_tool() -> Result<()> {
        let tool = Tool {
            name: "hover".to_string(),
            description: Some("Get hover info".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": "integer" },
                    "character": { "type": "integer" }
                },
                "required": ["file", "line", "character"]
            }),
        };

        let json = serde_json::to_string(&tool)?;
        assert!(json.contains("inputSchema"));
        assert!(json.contains("hover"));
        Ok(())
    }

    #[test]
    fn test_call_tool_result_text() -> Result<()> {
        let result = CallToolResult::text("Hello, world!");
        let json = serde_json::to_string(&result)?;
        assert!(json.contains("Hello, world!"));
        assert!(!json.contains("isError"));
        Ok(())
    }

    #[test]
    fn test_call_tool_result_error() -> Result<()> {
        let result = CallToolResult::error("Something went wrong");
        let json = serde_json::to_string(&result)?;
        assert!(json.contains("isError"));
        assert!(json.contains("true"));
        Ok(())
    }

    #[test]
    fn test_response_success() -> Result<()> {
        let resp = Response::success(RequestId::Number(1), serde_json::json!({"ok": true}))?;
        let json = serde_json::to_string(&resp)?;
        assert!(json.contains("result"));
        assert!(!json.contains("error"));
        Ok(())
    }

    #[test]
    fn test_response_error() -> Result<()> {
        let resp = Response::error(RequestId::Number(1), METHOD_NOT_FOUND, "Unknown method");
        let json = serde_json::to_string(&resp)?;
        assert!(json.contains("error"));
        assert!(json.contains("-32601"));
        assert!(!json.contains("result"));
        Ok(())
    }

    #[test]
    fn test_serialize_request() -> Result<()> {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::String("catenary-0".to_string()),
            method: "roots/list".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req)?;
        assert!(json.contains("roots/list"));
        assert!(json.contains("catenary-0"));
        Ok(())
    }

    /// Regression: the MCP TypeScript SDK rejects `"params": null` with a
    /// ZodError ("expected object, received null"). Requests and notifications
    /// with no params must omit the field entirely instead of serializing null.
    #[test]
    fn test_none_params_omitted_not_null() -> Result<()> {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::String("catenary-0".to_string()),
            method: "roots/list".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req)?;
        assert!(
            !json.contains("params"),
            "Request with params: None must omit the field, got: {json}"
        );

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/tools/list_changed".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&notification)?;
        assert!(
            !json.contains("params"),
            "Notification with params: None must omit the field, got: {json}"
        );

        Ok(())
    }

    #[test]
    fn test_deserialize_response_success() -> Result<()> {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"roots": []}
        }"#;
        let resp: Response = serde_json::from_str(json)?;
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        Ok(())
    }

    #[test]
    fn test_deserialize_response_error() -> Result<()> {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "error": {"code": -32601, "message": "not found"}
        }"#;
        let resp: Response = serde_json::from_str(json)?;
        assert!(resp.result.is_none());
        let err = resp.error.as_ref().context("missing error")?;
        assert_eq!(err.code, METHOD_NOT_FOUND);
        Ok(())
    }

    #[test]
    fn test_deserialize_root_with_name() -> Result<()> {
        let json = r#"{"uri": "file:///tmp/project", "name": "My Project"}"#;
        let root: Root = serde_json::from_str(json)?;
        assert_eq!(root.uri, "file:///tmp/project");
        assert_eq!(root.name.as_deref(), Some("My Project"));
        Ok(())
    }

    #[test]
    fn test_deserialize_root_without_name() -> Result<()> {
        let json = r#"{"uri": "file:///tmp/project"}"#;
        let root: Root = serde_json::from_str(json)?;
        assert_eq!(root.uri, "file:///tmp/project");
        assert!(root.name.is_none());
        Ok(())
    }

    #[test]
    fn test_deserialize_roots_list_result() -> Result<()> {
        let json = r#"{
            "roots": [
                {"uri": "file:///tmp/a", "name": "A"},
                {"uri": "file:///tmp/b"}
            ]
        }"#;
        let result: RootsListResult = serde_json::from_str(json)?;
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots[0].uri, "file:///tmp/a");
        assert_eq!(result.roots[1].name, None);
        Ok(())
    }
}
