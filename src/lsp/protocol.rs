// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use serde::{Deserialize, Serialize};

const fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

/// An LSP request message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RequestMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID.
    pub id: RequestId,
    /// The method name.
    pub method: String,
    /// The request parameters.
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

/// An LSP response message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID, if any.
    pub id: Option<RequestId>,
    /// The result of the request, if successful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The error, if the request failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// An LSP notification message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NotificationMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The method name.
    pub method: String,
    /// The notification parameters.
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

/// An LSP request or response ID.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric ID.
    Number(i64),
    /// A string ID.
    String(String),
}

/// An LSP response error.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseError {
    /// The error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        Self::Number(n)
    }
}

/// Helper to parse the Content-Length header and body from a buffer.
///
/// # Errors
///
/// Returns an error if:
/// - Headers are not valid UTF-8.
/// - Content-Length is invalid or missing.
/// - The body is not valid UTF-8.
pub fn try_parse_message(buffer: &mut BytesMut) -> Result<Option<String>> {
    let mut headers_end = None;
    let mut content_length = None;

    // Scan for \r\n\r\n
    for i in 0..buffer.len().saturating_sub(3) {
        if &buffer[i..i + 4] == b"\r\n\r\n" {
            headers_end = Some(i + 4);

            // Parse headers
            let headers_str =
                std::str::from_utf8(&buffer[0..i]).context("Failed to parse headers as UTF-8")?;

            for line in headers_str.lines() {
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() == 2 {
                        content_length = Some(parts[1].trim().parse::<usize>()?);
                    }
                }
            }
            break;
        }
    }

    if let (Some(header_len), Some(content_len)) = (headers_end, content_length) {
        let total_len = header_len + content_len;

        if buffer.len() >= total_len {
            buffer.advance(header_len);
            let message_bytes = buffer.split_to(content_len);
            let message = String::from_utf8(message_bytes.to_vec())?;
            return Ok(Some(message));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_complete_message() -> Result<()> {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, Some(body.to_string()));
        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_incomplete_header() -> Result<()> {
        let mut buffer = BytesMut::from("Content-Length: 10\r\n");
        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_parse_incomplete_body() -> Result<()> {
        let mut buffer = BytesMut::from("Content-Length: 100\r\n\r\n{\"partial\":");
        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_parse_multiple_messages() -> Result<()> {
        let body1 = r#"{"jsonrpc":"2.0","id":1}"#;
        let body2 = r#"{"jsonrpc":"2.0","id":2}"#;
        let raw = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            body1.len(),
            body1,
            body2.len(),
            body2
        );
        let mut buffer = BytesMut::from(raw.as_str());

        let result1 = try_parse_message(&mut buffer)?;
        assert_eq!(result1, Some(body1.to_string()));

        let result2 = try_parse_message(&mut buffer)?;
        assert_eq!(result2, Some(body2.to_string()));

        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_case_insensitive_header() -> Result<()> {
        let body = r#"{"test":true}"#;
        let raw = format!("content-length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, Some(body.to_string()));
        Ok(())
    }

    #[test]
    fn test_request_id_number() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":42,"method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json)?;
        assert_eq!(msg.id, RequestId::Number(42));
        Ok(())
    }

    #[test]
    fn test_request_id_string() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json)?;
        assert_eq!(msg.id, RequestId::String("abc-123".to_string()));
        Ok(())
    }

    #[test]
    fn test_response_with_result() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        assert!(msg.result.is_some());
        assert!(msg.error.is_none());
        Ok(())
    }

    #[test]
    fn test_response_with_error() -> Result<()> {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        assert!(msg.result.is_none());
        assert!(msg.error.is_some());
        assert_eq!(msg.error.context("missing error")?.code, -32600);
        Ok(())
    }

    #[test]
    fn test_response_null_result() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        // null deserializes to None for Option<Value>
        assert!(msg.result.is_none());
        Ok(())
    }

    #[test]
    fn test_notification_no_id() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        let msg: NotificationMessage = serde_json::from_str(json)?;
        assert_eq!(msg.method, "initialized");
        Ok(())
    }
}
