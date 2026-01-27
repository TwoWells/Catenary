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

use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use serde::{Deserialize, Serialize};

fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RequestMessage {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseMessage {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NotificationMessage {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

/// Helper to parse the Content-Length header and body from a buffer
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
    fn test_parse_complete_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result, Some(body.to_string()));
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_parse_incomplete_header() {
        let mut buffer = BytesMut::from("Content-Length: 10\r\n");
        let result = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_incomplete_body() {
        let mut buffer = BytesMut::from("Content-Length: 100\r\n\r\n{\"partial\":");
        let result = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_multiple_messages() {
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

        let result1 = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result1, Some(body1.to_string()));

        let result2 = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result2, Some(body2.to_string()));

        assert!(buffer.is_empty());
    }

    #[test]
    fn test_parse_case_insensitive_header() {
        let body = r#"{"test":true}"#;
        let raw = format!("content-length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer).unwrap();
        assert_eq!(result, Some(body.to_string()));
    }

    #[test]
    fn test_request_id_number() {
        let json = r#"{"jsonrpc":"2.0","id":42,"method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, RequestId::Number(42));
    }

    #[test]
    fn test_request_id_string() {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, RequestId::String("abc-123".to_string()));
    }

    #[test]
    fn test_response_with_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let msg: ResponseMessage = serde_json::from_str(json).unwrap();
        assert!(msg.result.is_some());
        assert!(msg.error.is_none());
    }

    #[test]
    fn test_response_with_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let msg: ResponseMessage = serde_json::from_str(json).unwrap();
        assert!(msg.result.is_none());
        assert!(msg.error.is_some());
        assert_eq!(msg.error.unwrap().code, -32600);
    }

    #[test]
    fn test_response_null_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let msg: ResponseMessage = serde_json::from_str(json).unwrap();
        // null deserializes to None for Option<Value>
        assert!(msg.result.is_none());
    }

    #[test]
    fn test_notification_no_id() {
        let json = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        let msg: NotificationMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.method, "initialized");
    }
}
