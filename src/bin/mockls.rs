// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! A configurable mock LSP server for testing.
//!
//! Speaks the LSP protocol over stdin/stdout using Content-Length framed
//! JSON-RPC. CLI flags control capabilities, timing, and failure modes.
//! No tokio — uses `std::thread` for deferred notifications.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Mock LSP server for integration testing.
#[derive(Parser, Debug)]
#[command(name = "mockls")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI flags are inherently boolean"
)]
struct Args {
    /// Advertise workspace folder support with change notifications.
    #[arg(long)]
    workspace_folders: bool,

    /// Emit progress begin/end after initialized (milliseconds).
    #[arg(long, default_value_t = 0)]
    indexing_delay: u64,

    /// Sleep before every response (milliseconds).
    #[arg(long, default_value_t = 0)]
    response_delay: u64,

    /// Delay before publishing diagnostics (milliseconds).
    #[arg(long, default_value_t = 0)]
    diagnostics_delay: u64,

    /// Never publish diagnostics.
    #[arg(long)]
    no_diagnostics: bool,

    /// Only publish diagnostics on `didSave`, not `didOpen`/`didChange`.
    #[arg(long)]
    diagnostics_on_save: bool,

    /// Close stdout after n responses (simulate crash).
    #[arg(long)]
    drop_after: Option<u64>,

    /// Never respond to this method (repeatable).
    #[arg(long)]
    hang_on: Vec<String>,

    /// Return `InternalError` for this method (repeatable).
    #[arg(long)]
    fail_on: Vec<String>,

    /// Send workspace/configuration request after initialize.
    #[arg(long)]
    send_configuration_request: bool,
}

/// A JSON-RPC request.
#[derive(Debug, Deserialize)]
struct Request {
    #[allow(dead_code, reason = "Required by JSON-RPC protocol")]
    jsonrpc: String,
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC response.
#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// Thread-safe writer handle. Wraps `std::io::Stdout` for production,
/// or a shared `Vec<u8>` for tests.
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Create a writer that forwards to stdout.
fn stdout_writer() -> Writer {
    Arc::new(Mutex::new(Box::new(std::io::stdout())))
}

#[cfg(test)]
fn buffer_writer() -> (Writer, Arc<Mutex<Vec<u8>>>) {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer: Box<dyn Write + Send> = Box::new(SharedVecWriter(buf.clone()));
    (Arc::new(Mutex::new(writer)), buf)
}

/// Write adapter for `Arc<Mutex<Vec<u8>>>` used in tests.
#[cfg(test)]
struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedVecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Shared state for the mock server.
struct MockServer {
    args: Args,
    documents: HashMap<String, String>,
    response_count: u64,
    writer: Writer,
    shutdown_flag: Arc<AtomicBool>,
    next_request_id: Arc<AtomicU64>,
}

impl MockServer {
    fn new(args: Args, writer: Writer) -> Self {
        Self {
            args,
            documents: HashMap::new(),
            response_count: 0,
            writer,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            next_request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Run the server, reading from the given reader.
    fn run(&mut self, reader: &mut dyn Read) {
        let mut buffer = Vec::new();
        let mut temp = [0u8; 4096];

        loop {
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            match reader.read(&mut temp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.extend_from_slice(&temp[..n]),
            }

            while let Some((message, consumed)) = try_parse_message(&buffer) {
                buffer.drain(..consumed);

                let Ok(request) = serde_json::from_str::<Request>(&message) else {
                    continue;
                };

                self.handle_message(request);
            }
        }
    }

    fn handle_message(&mut self, request: Request) {
        let Some(method) = request.method.clone() else {
            return;
        };

        if request.id.is_some() {
            self.handle_request(&method, request);
        } else {
            self.handle_notification(&method, &request.params);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Method dispatch requires handling many LSP methods"
    )]
    fn handle_request(&mut self, method: &str, request: Request) {
        let Some(id) = request.id else { return };

        // Check hang_on — never respond
        if self.args.hang_on.iter().any(|m| m == method) {
            return;
        }

        // Response delay
        if self.args.response_delay > 0 {
            std::thread::sleep(Duration::from_millis(self.args.response_delay));
        }

        // Check fail_on — return `InternalError`
        if self.args.fail_on.iter().any(|m| m == method) {
            self.send_response(&Response {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(RpcError {
                    code: -32603,
                    message: format!("mockls: configured to fail on {method}"),
                }),
            });
            return;
        }

        let result = match method {
            "initialize" => Some(self.handle_initialize()),
            "shutdown" => Some(Value::Null),
            "textDocument/hover" => self.handle_hover(&request.params),
            "textDocument/definition" => self.handle_definition(&request.params),
            "textDocument/references" => self.handle_references(&request.params),
            "textDocument/documentSymbol" => self.handle_document_symbols(&request.params),
            "workspace/symbol" => Some(self.handle_workspace_symbols(&request.params)),
            _ => {
                self.send_response(&Response {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: None,
                    error: Some(RpcError {
                        code: -32601,
                        message: format!("mockls: method not found: {method}"),
                    }),
                });
                return;
            }
        };

        self.send_response(&Response {
            jsonrpc: "2.0".to_string(),
            id,
            result,
            error: None,
        });

        if method == "initialize" && self.args.send_configuration_request {
            self.send_configuration_request();
        }
    }

    fn handle_notification(&mut self, method: &str, params: &Value) {
        match method {
            "initialized" => {
                if self.args.indexing_delay > 0 {
                    self.start_indexing_simulation();
                }
            }
            "textDocument/didOpen" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    let text = td.get("text").and_then(Value::as_str).unwrap_or_default();
                    self.documents.insert(uri.to_string(), text.to_string());

                    if !self.args.no_diagnostics && !self.args.diagnostics_on_save {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didChange" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if let Some(text) = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.last())
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        self.documents.insert(uri.to_string(), text.to_string());
                    }

                    if !self.args.no_diagnostics && !self.args.diagnostics_on_save {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didSave" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if !self.args.no_diagnostics {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    self.documents.remove(uri);
                }
            }
            "exit" => {
                self.shutdown_flag.store(true, Ordering::SeqCst);
                std::process::exit(0);
            }
            // workspace/didChangeWorkspaceFolders and all others are silently accepted
            _ => {}
        }
    }

    fn handle_initialize(&self) -> Value {
        let mut capabilities = serde_json::json!({
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": { "includeText": false }
            }
        });

        if self.args.workspace_folders {
            capabilities["workspace"] = serde_json::json!({
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": true
                }
            });
        }

        serde_json::json!({ "capabilities": capabilities })
    }

    fn handle_hover(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        Some(serde_json::json!({
            "contents": {
                "kind": "markdown",
                "value": format!("```\n{word}\n```")
            }
        }))
    }

    fn handle_definition(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        let def_patterns = [
            format!("fn {word}"),
            format!("function {word}"),
            format!("def {word}"),
            format!("let {word}"),
            format!("const {word}"),
            format!("var {word}"),
        ];

        for (line_idx, line_text) in content.lines().enumerate() {
            for pattern in &def_patterns {
                if let Some(col_idx) = line_text.find(pattern.as_str()) {
                    return Some(location_json(
                        uri,
                        line_idx,
                        col_idx,
                        col_idx + pattern.len(),
                    ));
                }
            }
        }

        // Fall back to first occurrence
        for (line_idx, line_text) in content.lines().enumerate() {
            if let Some(col_idx) = line_text.find(&word) {
                return Some(location_json(uri, line_idx, col_idx, col_idx + word.len()));
            }
        }

        None
    }

    fn handle_references(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        let mut locations = Vec::new();
        for (line_idx, line_text) in content.lines().enumerate() {
            let mut start = 0;
            while let Some(pos) = line_text[start..].find(&word) {
                let col_idx = start + pos;
                locations.push(location_json(uri, line_idx, col_idx, col_idx + word.len()));
                start = col_idx + word.len();
            }
        }

        Some(Value::Array(locations))
    }

    fn handle_document_symbols(&self, params: &Value) -> Option<Value> {
        let uri = params
            .get("textDocument")
            .and_then(|td| td.get("uri"))
            .and_then(Value::as_str)?;

        let content = self.documents.get(uri)?;
        Some(Value::Array(extract_symbols(content)))
    }

    fn handle_workspace_symbols(&self, params: &Value) -> Value {
        let query = params.get("query").and_then(Value::as_str).unwrap_or("");

        let mut all_symbols = Vec::new();
        for (uri, content) in &self.documents {
            for mut sym in extract_symbols(content) {
                let matches = sym
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| query.is_empty() || n.contains(query));

                if matches && let Some(range) = sym.get("range").cloned() {
                    if let Some(obj) = sym.as_object_mut() {
                        obj.insert(
                            "location".to_string(),
                            serde_json::json!({ "uri": uri, "range": range }),
                        );
                        obj.remove("range");
                        obj.remove("selectionRange");
                    }
                    all_symbols.push(sym);
                }
            }
        }

        Value::Array(all_symbols)
    }

    fn publish_diagnostics(&self, uri: &str) {
        let delay = self.args.diagnostics_delay;
        let uri_owned = uri.to_string();
        let writer = self.writer.clone();

        if delay > 0 {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                send_diagnostics_notification(&writer, &uri_owned);
            });
        } else {
            send_diagnostics_notification(&self.writer, &uri_owned);
        }
    }

    fn start_indexing_simulation(&self) {
        let delay = self.args.indexing_delay;
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();

        std::thread::spawn(move || {
            let token = "mockls-indexing";

            let req_id = next_id.fetch_add(1, Ordering::SeqCst);
            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": "window/workDoneProgress/create",
                    "params": { "token": token }
                }),
            );

            std::thread::sleep(Duration::from_millis(50));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
                    }
                }),
            );

            std::thread::sleep(Duration::from_millis(delay));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "end", "message": "Indexing complete" }
                    }
                }),
            );
        });
    }

    fn send_configuration_request(&self) {
        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        send_message(
            &self.writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "workspace/configuration",
                "params": { "items": [{ "section": "mockls" }] }
            }),
        );
    }

    fn send_response(&mut self, response: &Response) {
        let Ok(json) = serde_json::to_string(response) else {
            return;
        };

        write_framed(&self.writer, &json);

        self.response_count += 1;

        if let Some(max) = self.args.drop_after
            && self.response_count >= max
        {
            std::process::exit(1);
        }
    }
}

/// Extract `(uri, line, col)` from a `textDocument/position` params object.
fn extract_position(params: &Value) -> Option<(&str, usize, usize)> {
    let uri = params
        .get("textDocument")
        .and_then(|td| td.get("uri"))
        .and_then(Value::as_str)?;
    let line = usize::try_from(
        params
            .get("position")
            .and_then(|p| p.get("line"))
            .and_then(Value::as_u64)?,
    )
    .ok()?;
    let col = usize::try_from(
        params
            .get("position")
            .and_then(|p| p.get("character"))
            .and_then(Value::as_u64)?,
    )
    .ok()?;
    Some((uri, line, col))
}

/// Build a JSON `Location` object.
fn location_json(uri: &str, line: usize, start: usize, end: usize) -> Value {
    serde_json::json!({
        "uri": uri,
        "range": {
            "start": { "line": line, "character": start },
            "end": { "line": line, "character": end }
        }
    })
}

/// Write a Content-Length framed JSON string.
fn write_framed(writer: &Writer, json: &str) {
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    let Ok(mut w) = writer.lock() else { return };
    let _ = w.write_all(header.as_bytes());
    let _ = w.write_all(json.as_bytes());
    let _ = w.flush();
}

/// Send a JSON-RPC message to the client.
fn send_message(writer: &Writer, value: &Value) {
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    write_framed(writer, &json);
}

/// Send a `publishDiagnostics` notification.
fn send_diagnostics_notification(writer: &Writer, uri: &str) {
    send_message(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "severity": 2,
                    "source": "mockls",
                    "message": "mockls: mock diagnostic"
                }]
            }
        }),
    );
}

/// Parse a Content-Length framed message from a buffer.
/// Returns the message string and the number of bytes consumed.
fn try_parse_message(buffer: &[u8]) -> Option<(String, usize)> {
    let header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&buffer[..header_end]).ok()?;

    let mut content_length: Option<usize> = None;
    for line in headers.lines() {
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line
                .split_once(':')
                .and_then(|(_, v)| v.trim().parse().ok());
        }
    }

    let content_length = content_length?;
    let total = header_end + 4 + content_length;

    if buffer.len() < total {
        return None;
    }

    let body = std::str::from_utf8(&buffer[header_end + 4..total]).ok()?;
    Some((body.to_string(), total))
}

/// Extract the word at a given line and column from content.
fn extract_word(content: &str, line: usize, col: usize) -> Option<String> {
    let line_text = content.lines().nth(line)?;

    if col >= line_text.len() {
        return None;
    }

    let bytes = line_text.as_bytes();

    let start = (0..=col)
        .rev()
        .find(|&i| !is_word_char(bytes[i]))
        .map_or(0, |i| i + 1);

    let end = (col..bytes.len())
        .find(|&i| !is_word_char(bytes[i]))
        .unwrap_or(bytes.len());

    if start >= end {
        return None;
    }

    Some(line_text[start..end].to_string())
}

const fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract symbol definitions from content.
fn extract_symbols(content: &str) -> Vec<Value> {
    let mut symbols = Vec::new();

    for (line_idx, line_text) in content.lines().enumerate() {
        let trimmed = line_text.trim_start();
        let (kind_num, prefix_len) = if trimmed.starts_with("fn ") {
            (12, 3)
        } else if trimmed.starts_with("function ") {
            (12, 9)
        } else if trimmed.starts_with("def ") {
            (12, 4)
        } else if trimmed.starts_with("let ") {
            (13, 4)
        } else if trimmed.starts_with("const ") {
            (14, 6)
        } else if trimmed.starts_with("var ") {
            (13, 4)
        } else {
            continue;
        };

        let after_keyword = &trimmed[prefix_len..];
        let name: String = after_keyword
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        if name.is_empty() {
            continue;
        }

        let indent = line_text.len() - trimmed.len();
        let col_start = indent + prefix_len;

        symbols.push(serde_json::json!({
            "name": name,
            "kind": kind_num,
            "range": {
                "start": { "line": line_idx, "character": indent },
                "end": { "line": line_idx, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line_idx, "character": col_start },
                "end": { "line": line_idx, "character": col_start + name.len() }
            }
        }));
    }

    symbols
}

fn main() {
    let args = Args::parse();
    let writer = stdout_writer();
    let mut server = MockServer::new(args, writer);
    let mut stdin = std::io::stdin().lock();
    server.run(&mut stdin);
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Tests use expect/unwrap for clear failure messages"
)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn default_args() -> Args {
        Args {
            workspace_folders: false,
            indexing_delay: 0,
            response_delay: 0,
            diagnostics_delay: 0,
            no_diagnostics: false,
            diagnostics_on_save: false,
            drop_after: None,
            hang_on: vec![],
            fail_on: vec![],
            send_configuration_request: false,
        }
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    fn extract_messages(data: &[u8]) -> Vec<Value> {
        let mut messages = Vec::new();
        let mut buf = data.to_vec();
        while let Some((msg, consumed)) = try_parse_message(&buf) {
            if let Ok(v) = serde_json::from_str::<Value>(&msg) {
                messages.push(v);
            }
            buf.drain(..consumed);
        }
        messages
    }

    fn run_server_with(args: Args, input: &[u8]) -> Vec<Value> {
        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(args, writer);
        let mut reader = Cursor::new(input.to_vec());
        server.run(&mut reader);
        let data = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        extract_messages(&data)
    }

    fn run_server_wait(args: Args, input: &[u8], wait_ms: u64) -> Vec<Value> {
        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(args, writer);
        let mut reader = Cursor::new(input.to_vec());
        server.run(&mut reader);
        std::thread::sleep(Duration::from_millis(wait_ms));
        let data = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        extract_messages(&data)
    }

    fn initialize_request(id: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": "file:///tmp/test"
            }
        })
        .to_string()
    }

    fn shutdown_request(id: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null
        })
        .to_string()
    }

    fn did_open_notification(uri: &str, text: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "shellscript",
                    "version": 1,
                    "text": text
                }
            }
        })
        .to_string()
    }

    fn hover_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn definition_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    #[test]
    fn test_initialize_response_valid() {
        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(default_args(), &input);

        assert!(!messages.is_empty(), "Expected at least one response");
        let resp = &messages[0];
        assert_eq!(resp["id"], 1);
        assert!(resp["result"].is_object(), "Expected result object");
        assert!(
            resp["result"]["capabilities"].is_object(),
            "Expected capabilities"
        );
        assert!(resp["error"].is_null(), "Expected no error");

        let caps = &resp["result"]["capabilities"];
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(caps["definitionProvider"], true);
        assert_eq!(caps["referencesProvider"], true);
        assert_eq!(caps["documentSymbolProvider"], true);
    }

    #[test]
    fn test_initialize_workspace_folders_capability() {
        let mut args = default_args();
        args.workspace_folders = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);
        let ws = &messages[0]["result"]["capabilities"]["workspace"]["workspaceFolders"];
        assert_eq!(ws["supported"], true);
        assert_eq!(ws["changeNotifications"], true);
    }

    #[test]
    fn test_hover_response_structure() {
        let uri = "file:///tmp/test.sh";
        let text = "#!/bin/bash\necho hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&hover_request(2, uri, 1, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        assert!(hover["error"].is_null(), "Expected no error");
        let result = &hover["result"];
        assert!(result.is_object());
        assert_eq!(result["contents"]["kind"], "markdown");
        let value = result["contents"]["value"].as_str().unwrap_or("");
        assert!(value.contains("echo"), "Expected 'echo' in hover content");
    }

    #[test]
    fn test_definition_response_structure() {
        let uri = "file:///tmp/test.sh";
        let text = "fn my_func() {}\nmy_func\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&definition_request(2, uri, 1, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");

        assert!(def["error"].is_null(), "Expected no error");
        let result = &def["result"];
        assert_eq!(result["uri"], uri);
        assert_eq!(result["range"]["start"]["line"], 0);
    }

    #[test]
    fn test_diagnostics_notification_structure() {
        let uri = "file:///tmp/test.sh";
        let text = "#!/bin/bash\necho hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(default_args(), &input);

        let diag = messages
            .iter()
            .find(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .expect("publishDiagnostics notification");

        let params = &diag["params"];
        assert_eq!(params["uri"], uri);
        let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
        assert!(!diagnostics.is_empty());

        let d = &diagnostics[0];
        assert_eq!(d["severity"], 2);
        assert_eq!(d["source"], "mockls");
        assert!(
            d["message"]
                .as_str()
                .unwrap_or("")
                .contains("mock diagnostic")
        );
    }

    #[test]
    fn test_progress_sequence() {
        let mut args = default_args();
        args.indexing_delay = 100;

        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })
        .to_string();

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&initialized));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_wait(args, &input, 250);

        let has_create = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("window/workDoneProgress/create")
        });
        assert!(
            has_create,
            "Expected workDoneProgress/create. Got: {messages:?}"
        );

        let has_begin = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "begin"
        });
        assert!(has_begin, "Expected $/progress begin. Got: {messages:?}");

        let has_end = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "end"
        });
        assert!(has_end, "Expected $/progress end. Got: {messages:?}");
    }

    #[test]
    fn test_content_length_framing() {
        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(default_args(), writer);
        let mut reader = Cursor::new(input);
        server.run(&mut reader);

        let output_str = {
            let data = buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            String::from_utf8_lossy(&data).into_owned()
        };
        let mut remaining = output_str.as_str();

        let mut count = 0;
        while !remaining.is_empty() {
            let header_end = remaining.find("\r\n\r\n").expect("Content-Length header");
            let headers = &remaining[..header_end];

            let cl_line = headers
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .expect("Content-Length header line");

            let cl: usize = cl_line
                .split_once(':')
                .expect("colon in header")
                .1
                .trim()
                .parse()
                .expect("valid content-length");

            let body_start = header_end + 4;
            let body = &remaining[body_start..body_start + cl];

            let _: Value = serde_json::from_str(body).expect("valid JSON body");

            remaining = &remaining[body_start + cl..];
            count += 1;
        }

        assert!(count >= 2, "Expected at least 2 framed messages");
    }

    #[test]
    fn test_request_id_echo() {
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "initialize",
            "params": { "processId": null, "capabilities": {}, "rootUri": null }
        })
        .to_string();
        let shutdown = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "string-id",
            "method": "shutdown",
            "params": null
        })
        .to_string();

        let mut input = frame(&init);
        input.extend(frame(&shutdown));

        let messages = run_server_with(default_args(), &input);

        assert_eq!(messages[0]["id"], 42, "Init should echo numeric id");

        let shutdown_resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some("string-id"));
        assert!(shutdown_resp.is_some(), "Shutdown should echo string id");
    }
}
