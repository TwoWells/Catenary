// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! A configurable mock LSP server for testing.
//!
//! Speaks the LSP protocol over stdin/stdout using Content-Length framed
//! JSON-RPC. CLI flags control capabilities, timing, and failure modes.
//! No tokio — uses `std::thread` for deferred notifications.
//!
//! Code actions: by default, returns one quickfix action per diagnostic
//! (source "mockls") plus a `refactor` action (to exercise kind filtering).
//! `--no-code-actions` omits the `codeActionProvider` capability entirely.
//! `--multi-fix` returns two quickfix actions per diagnostic.

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
    /// Language name. Used as the file extension for --scan-roots filtering.
    #[arg()]
    name: String,

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

    /// Include `version` field in `publishDiagnostics` notifications.
    #[arg(long)]
    publish_version: bool,

    /// Send progress tokens around diagnostic computation on `didChange`
    /// (simulates cargo clippy progress).
    #[arg(long)]
    progress_on_change: bool,

    /// Burn CPU for N milliseconds after `didChange` without sending any
    /// notifications (simulates a server doing work without progress).
    #[arg(long)]
    cpu_busy: Option<u64>,

    /// Command to spawn on `didSave` (simulates flycheck/cargo check).
    /// Wraps the subprocess in a `$/progress` Begin/End bracket.
    /// Use with mockc to create the real scheduling pattern:
    /// `--flycheck-command "mockc --ticks 20"`
    #[arg(long)]
    flycheck_command: Option<String>,

    /// Include `textDocumentSync.save` in `ServerCapabilities`.
    /// Required for the server to receive `textDocument/didSave`.
    #[arg(long)]
    advertise_save: bool,

    /// Write every received notification method to a JSONL file.
    /// Each line is `{"method":"...","uri":"..."}` (uri if available).
    /// Tests read after shutdown to verify notification delivery.
    #[arg(long)]
    notification_log: Option<String>,

    /// Return `ContentModified` (-32801) on the first `textDocument/definition`
    /// request, then succeed on retry. Tests the retry path.
    #[arg(long)]
    content_modified_once: bool,

    /// Burn CPU for N milliseconds on `workspace/didChangeWorkspaceFolders`.
    /// No progress tokens are sent. Tests `wait_ready` failure detection.
    #[arg(long)]
    cpu_on_workspace_change: Option<u64>,

    /// Burn CPU for N milliseconds on `initialized` notification (before
    /// indexing simulation). Tests warmup observation in `is_ready()`.
    #[arg(long)]
    cpu_on_initialized: Option<u64>,

    /// Write the `initialize` request params JSON to the specified file.
    /// Tests can read this to verify client capabilities.
    #[arg(long)]
    log_init_params: Option<String>,

    /// Override the number of ticks passed to the flycheck subprocess.
    /// Appends `--ticks <N>` to the flycheck command args.
    #[arg(long)]
    flycheck_ticks: Option<u64>,

    /// Scan workspace roots on initialize and workspace folder changes.
    /// Indexes all text files into `documents`, making them visible to
    /// `workspace/symbol` without a prior `didOpen`.
    #[arg(long)]
    scan_roots: bool,

    /// Never return code actions (omit `codeActionProvider` capability).
    #[arg(long)]
    no_code_actions: bool,

    /// Return multiple quickfix actions per diagnostic.
    #[arg(long)]
    multi_fix: bool,

    /// Advertise `workspaceSymbol/resolve` support. When set, `workspace/symbol`
    /// returns URI-only locations (no range) and `workspaceSymbol/resolve`
    /// returns full locations.
    #[arg(long)]
    resolve_provider: bool,

    /// Return empty results for `workspace/symbol` with empty query.
    /// Forces the fallback to per-query lookup.
    #[arg(long)]
    no_empty_query: bool,

    /// Cap `workspace/symbol("")` results to N (simulates truncation).
    /// Non-empty queries and hover return full results.
    #[arg(long)]
    symbol_limit: Option<usize>,

    /// Return the literal word under the cursor for hover, without resolving
    /// keywords to the following symbol name. Simulates real LSP behavior where
    /// hovering on `fn` returns keyword docs, not the function's signature.
    #[arg(long)]
    literal_keyword_hover: bool,
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
    /// Mock type map: `symbol_name → type_name` extracted from `: TypeName` annotations.
    types: HashMap<String, String>,
    /// Import map: `(document_uri, imported_name) → source_file_fragment`.
    /// Parsed from `from <file> import <name>` lines.
    imports: HashMap<(String, String), String>,
    /// Tracks the document version from `didOpen`/`didChange` per URI.
    versions: HashMap<String, i32>,
    response_count: u64,
    writer: Writer,
    shutdown_flag: Arc<AtomicBool>,
    next_request_id: Arc<AtomicU64>,
    /// Optional notification log file for test verification.
    notification_log: Option<std::fs::File>,
    /// Whether the first definition request has been seen (for `--content-modified-once`).
    definition_failed_once: bool,
    /// Workspace roots parsed from `initialize` params.
    workspace_roots: Vec<String>,
}

impl MockServer {
    fn new(args: Args, writer: Writer) -> Self {
        let notification_log = args
            .notification_log
            .as_ref()
            .and_then(|path| std::fs::File::create(path).ok());

        Self {
            args,
            documents: HashMap::new(),
            types: HashMap::new(),
            imports: HashMap::new(),
            versions: HashMap::new(),
            response_count: 0,
            writer,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            next_request_id: Arc::new(AtomicU64::new(1)),
            notification_log,
            definition_failed_once: false,
            workspace_roots: Vec::new(),
        }
    }

    /// Recursively scans a directory, indexing `.mock` files into `self.documents`.
    fn scan_directory(&mut self, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden directories
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                self.scan_directory(&path);
            } else if path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some(self.args.name.as_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let abs = path.to_string_lossy();
                let uri = format!("file://{abs}");
                self.documents.insert(uri, content);
            }
        }
    }

    /// Rebuilds the type map from all open documents.
    fn rebuild_types(&mut self) {
        self.types.clear();
        for content in self.documents.values() {
            self.types.extend(extract_types(content));
        }
    }

    /// Rebuilds the import map from all open documents.
    /// Parses `from <file> import <name>` lines.
    fn rebuild_imports(&mut self) {
        self.imports.clear();
        for (uri, content) in &self.documents {
            for line_text in content.lines() {
                let trimmed = line_text.trim_start();
                if let Some(rest) = trimmed.strip_prefix("from ") {
                    let mut parts = rest.split_whitespace();
                    let Some(file_fragment) = parts.next() else {
                        continue;
                    };
                    if parts.next() != Some("import") {
                        continue;
                    }
                    let Some(name) = parts.next() else {
                        continue;
                    };
                    self.imports
                        .insert((uri.clone(), name.to_string()), file_fragment.to_string());
                }
            }
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
            "initialize" => {
                if let Some(ref path) = self.args.log_init_params {
                    let json = serde_json::to_string_pretty(&request.params).unwrap_or_default();
                    let _ = std::fs::write(path, json);
                }
                Some(self.handle_initialize(&request.params))
            }
            "shutdown" => Some(Value::Null),
            "textDocument/hover" => self.handle_hover(&request.params),
            "textDocument/definition" => {
                if self.args.content_modified_once && !self.definition_failed_once {
                    self.definition_failed_once = true;
                    self.send_response(&Response {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(RpcError {
                            code: -32801,
                            message: "ContentModified".to_string(),
                        }),
                    });
                    return;
                }
                self.handle_definition(&request.params)
            }
            "textDocument/typeDefinition" => self.handle_type_definition(&request.params),
            "textDocument/references" | "textDocument/implementation" => {
                self.handle_references(&request.params)
            }
            "textDocument/documentSymbol" => self.handle_document_symbols(&request.params),
            "workspace/symbol" => Some(self.handle_workspace_symbols(&request.params)),
            "workspaceSymbol/resolve" => self.handle_workspace_symbol_resolve(&request.params),
            "textDocument/prepareCallHierarchy" => {
                self.handle_call_hierarchy_prepare(&request.params)
            }
            "callHierarchy/incomingCalls" => self.handle_incoming_calls(&request.params),
            "textDocument/prepareTypeHierarchy" => {
                self.handle_type_hierarchy_prepare(&request.params)
            }
            "typeHierarchy/subtypes" => self.handle_type_hierarchy_subtypes(&request.params),
            "textDocument/codeAction" => Some(self.handle_code_action(&request.params)),
            "textDocument/prepareRename" => self.handle_prepare_rename(&request.params),
            "callHierarchy/outgoingCalls" | "typeHierarchy/supertypes" => {
                Some(Value::Array(Vec::new()))
            }
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

    #[allow(
        clippy::too_many_lines,
        reason = "Notification dispatch handles many LSP methods with scan-roots logic"
    )]
    fn handle_notification(&mut self, method: &str, params: &Value) {
        // Log notification if configured
        if let Some(ref mut log) = self.notification_log {
            let uri = params
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let entry = serde_json::json!({"method": method, "uri": uri});
            let _ = writeln!(log, "{entry}");
        }

        match method {
            "initialized" => {
                if let Some(busy_ms) = self.args.cpu_on_initialized {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(busy_ms) {
                        std::hint::spin_loop();
                    }
                }
                if self.args.indexing_delay > 0 {
                    self.start_indexing_simulation();
                }
            }
            "textDocument/didOpen" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    let text = td.get("text").and_then(Value::as_str).unwrap_or_default();
                    let version = td
                        .get("version")
                        .and_then(Value::as_i64)
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(1);
                    self.documents.insert(uri.to_string(), text.to_string());
                    self.versions.insert(uri.to_string(), version);
                    self.rebuild_types();
                    self.rebuild_imports();

                    if !self.args.no_diagnostics && !self.args.diagnostics_on_save {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didChange" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    let version = td
                        .get("version")
                        .and_then(Value::as_i64)
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(1);
                    self.versions.insert(uri.to_string(), version);
                    if let Some(text) = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.last())
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        self.documents.insert(uri.to_string(), text.to_string());
                        self.rebuild_types();
                        self.rebuild_imports();
                    }

                    // Simulate CPU-bound work without any notifications
                    if let Some(busy_ms) = self.args.cpu_busy {
                        let start = std::time::Instant::now();
                        while start.elapsed() < Duration::from_millis(busy_ms) {
                            std::hint::spin_loop();
                        }
                    }

                    if self.args.progress_on_change {
                        self.simulate_progress_around_diagnostics(uri);
                    } else if !self.args.no_diagnostics && !self.args.diagnostics_on_save {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didSave" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if let Some(ref cmd) = self.args.flycheck_command {
                        self.run_flycheck(uri, cmd);
                    } else if !self.args.no_diagnostics {
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
            "workspace/didChangeWorkspaceFolders" => {
                if let Some(busy_ms) = self.args.cpu_on_workspace_change {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(busy_ms) {
                        std::hint::spin_loop();
                    }
                }

                if self.args.scan_roots
                    && let Some(event) = params.get("event")
                {
                    // Remove documents from removed folders
                    if let Some(removed) = event.get("removed").and_then(Value::as_array) {
                        for folder in removed {
                            if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                                let path = uri.strip_prefix("file://").unwrap_or(uri);
                                self.workspace_roots.retain(|r| r != path);
                                let prefix = format!("file://{path}");
                                self.documents.retain(|k, _| !k.starts_with(&prefix));
                            }
                        }
                    }
                    // Scan added folders
                    if let Some(added) = event.get("added").and_then(Value::as_array) {
                        for folder in added {
                            if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                                let path = uri.strip_prefix("file://").unwrap_or(uri);
                                if !self.workspace_roots.contains(&path.to_string()) {
                                    self.workspace_roots.push(path.to_string());
                                }
                                self.scan_directory(std::path::Path::new(path));
                            }
                        }
                    }
                    self.rebuild_types();
                    self.rebuild_imports();
                }
            }
            // All other notifications are silently accepted
            _ => {}
        }
    }

    fn handle_initialize(&mut self, params: &Value) -> Value {
        // Parse workspace roots from initialize params
        let mut roots = Vec::new();
        if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            if !path.is_empty() {
                roots.push(path.to_string());
            }
        }
        if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
            for folder in folders {
                if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    if !path.is_empty() && !roots.contains(&path.to_string()) {
                        roots.push(path.to_string());
                    }
                }
            }
        }

        if self.args.scan_roots {
            for root in &roots {
                self.scan_directory(std::path::Path::new(root));
            }
            self.rebuild_types();
            self.rebuild_imports();
        }
        self.workspace_roots = roots;

        let mut text_doc_sync = serde_json::json!({
            "openClose": true,
            "change": 1
        });
        if self.args.advertise_save {
            text_doc_sync["save"] = serde_json::json!({ "includeText": false });
        }

        let workspace_symbol_value = if self.args.resolve_provider {
            serde_json::json!({ "resolveProvider": true })
        } else {
            serde_json::json!(true)
        };

        let mut capabilities = serde_json::json!({
            "hoverProvider": true,
            "definitionProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "implementationProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": workspace_symbol_value,
            "callHierarchyProvider": true,
            "typeHierarchyProvider": true,
            "renameProvider": { "prepareProvider": true },
            "textDocumentSync": text_doc_sync
        });

        if !self.args.no_code_actions {
            capabilities["codeActionProvider"] = serde_json::json!(true);
        }

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
        let word = if self.args.literal_keyword_hover {
            extract_word(content, line, col)?
        } else {
            extract_symbol_name(content, line, col)?
        };

        Some(serde_json::json!({
            "contents": {
                "kind": "markdown",
                "value": format!("```\n{word}\n```")
            }
        }))
    }

    fn handle_prepare_rename(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        let keywords = [
            "fn",
            "function",
            "def",
            "let",
            "const",
            "var",
            "struct",
            "class",
            "enum",
            "interface",
            "trait",
            "mod",
            "module",
            "type",
            "method",
            "field",
        ];
        if keywords.contains(&word.as_str()) {
            return None;
        }

        let line_text = content.lines().nth(line)?;
        let bytes = line_text.as_bytes();
        let start = (0..=col)
            .rev()
            .find(|&i| !is_word_char(bytes[i]))
            .map_or(0, |i| i + 1);
        let end = (col..bytes.len())
            .find(|&i| !is_word_char(bytes[i]))
            .unwrap_or(bytes.len());

        Some(serde_json::json!({
            "range": {
                "start": { "line": line, "character": start },
                "end": { "line": line, "character": end }
            },
            "placeholder": word
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
            format!("struct {word}"),
            format!("class {word}"),
            format!("enum {word}"),
            format!("interface {word}"),
            format!("trait {word}"),
            format!("mod {word}"),
            format!("module {word}"),
            format!("type {word}"),
            format!("method {word}"),
            format!("field {word}"),
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

        // Import-scoped resolution: if this file imports the word, search
        // only the source document for a definition pattern.
        if let Some(source_fragment) = self.imports.get(&(uri.to_string(), word.clone())) {
            for (doc_uri, doc_content) in &self.documents {
                if !doc_uri.contains(source_fragment.as_str()) {
                    continue;
                }
                for (line_idx, line_text) in doc_content.lines().enumerate() {
                    for pattern in &def_patterns {
                        if let Some(col_idx) = line_text.find(pattern.as_str()) {
                            return Some(location_json(
                                doc_uri,
                                line_idx,
                                col_idx,
                                col_idx + pattern.len(),
                            ));
                        }
                    }
                }
            }
        }

        // Cross-file: search all other documents for a definition pattern
        for (doc_uri, doc_content) in &self.documents {
            if doc_uri == uri {
                continue;
            }
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                for pattern in &def_patterns {
                    if let Some(col_idx) = line_text.find(pattern.as_str()) {
                        return Some(location_json(
                            doc_uri,
                            line_idx,
                            col_idx,
                            col_idx + pattern.len(),
                        ));
                    }
                }
            }
        }

        // Fall back to first occurrence in current document
        for (line_idx, line_text) in content.lines().enumerate() {
            if let Some(col_idx) = line_text.find(&word) {
                return Some(location_json(uri, line_idx, col_idx, col_idx + word.len()));
            }
        }

        // Cross-file fallback: first occurrence in any other document
        for (doc_uri, doc_content) in &self.documents {
            if doc_uri == uri {
                continue;
            }
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                if let Some(col_idx) = line_text.find(&word) {
                    return Some(location_json(
                        doc_uri,
                        line_idx,
                        col_idx,
                        col_idx + word.len(),
                    ));
                }
            }
        }

        None
    }

    fn handle_references(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_symbol_name(content, line, col)?;

        let mut locations = Vec::new();
        // Cross-file: search all documents for the word
        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                let mut start = 0;
                while let Some(pos) = line_text[start..].find(&word) {
                    let col_idx = start + pos;
                    locations.push(location_json(
                        doc_uri,
                        line_idx,
                        col_idx,
                        col_idx + word.len(),
                    ));
                    start = col_idx + word.len();
                }
            }
        }

        Some(Value::Array(locations))
    }

    fn handle_type_definition(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;

        // Look up name in type map. If not found, extract types from the
        // current line as a fallback (handles cases where the cursor lands
        // on a keyword and the name resolves but has no global type entry).
        let type_name = self.types.get(&name).cloned().or_else(|| {
            let line_text = content.lines().nth(line)?;
            let line_types = extract_types(line_text);
            line_types.into_values().next()
        })?;

        // Type declaration patterns
        let type_decl_patterns = [
            format!("struct {type_name}"),
            format!("class {type_name}"),
            format!("enum {type_name}"),
            format!("interface {type_name}"),
            format!("trait {type_name}"),
            format!("type {type_name}"),
        ];

        // Search all documents for the type declaration
        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                for pattern in &type_decl_patterns {
                    if let Some(col_idx) = line_text.find(pattern.as_str()) {
                        return Some(location_json(
                            doc_uri,
                            line_idx,
                            col_idx,
                            col_idx + pattern.len(),
                        ));
                    }
                }
            }
        }

        None
    }

    fn handle_call_hierarchy_prepare(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;
        let line_text = content.lines().nth(line)?;

        Some(serde_json::json!([{
            "name": name,
            "kind": 12,
            "uri": uri,
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            }
        }]))
    }

    fn handle_incoming_calls(&self, params: &Value) -> Option<Value> {
        let item = params.get("item")?;
        let name = item.get("name")?.as_str()?;
        let def_uri = item.get("uri")?.as_str()?;
        let def_line = item.get("range")?.get("start")?.get("line")?.as_u64()?;

        let mut calls = Vec::new();

        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                if doc_uri == def_uri && line_idx as u64 == def_line {
                    continue;
                }

                if !line_text.contains(name) {
                    continue;
                }

                if let Some((fn_name, fn_line)) = find_enclosing_function(content, line_idx) {
                    let fn_line_text = content.lines().nth(fn_line).unwrap_or("");
                    calls.push(serde_json::json!({
                        "from": {
                            "name": fn_name,
                            "kind": 12,
                            "uri": doc_uri,
                            "range": {
                                "start": { "line": fn_line, "character": 0 },
                                "end": { "line": fn_line, "character": fn_line_text.len() }
                            },
                            "selectionRange": {
                                "start": { "line": fn_line, "character": 0 },
                                "end": { "line": fn_line, "character": fn_line_text.len() }
                            }
                        },
                        "fromRanges": [{
                            "start": { "line": line_idx, "character": 0 },
                            "end": { "line": line_idx, "character": line_text.len() }
                        }]
                    }));
                }
            }
        }

        Some(Value::Array(calls))
    }

    fn handle_type_hierarchy_prepare(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;
        let line_text = content.lines().nth(line)?;

        let trimmed = line_text.trim_start();
        let kind: u32 = if trimmed.starts_with("interface ") || trimmed.starts_with("trait ") {
            11
        } else if trimmed.starts_with("class ") {
            5
        } else if trimmed.starts_with("enum ") {
            10
        } else {
            23
        };

        Some(serde_json::json!([{
            "name": name,
            "kind": kind,
            "uri": uri,
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            }
        }]))
    }

    fn handle_type_hierarchy_subtypes(&self, params: &Value) -> Option<Value> {
        let type_keywords = ["struct ", "class "];
        let mut subtypes = Vec::new();

        let _item = params.get("item")?;

        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                let trimmed = line_text.trim_start();
                for kw in &type_keywords {
                    if let Some(after_kw) = trimmed.strip_prefix(kw) {
                        let type_name: String = after_kw
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if !type_name.is_empty() {
                            let kind: u32 = if *kw == "struct " { 23 } else { 5 };
                            subtypes.push(serde_json::json!({
                                "name": type_name,
                                "kind": kind,
                                "uri": doc_uri,
                                "range": {
                                    "start": { "line": line_idx, "character": 0 },
                                    "end": { "line": line_idx, "character": line_text.len() }
                                },
                                "selectionRange": {
                                    "start": { "line": line_idx, "character": 0 },
                                    "end": { "line": line_idx, "character": line_text.len() }
                                }
                            }));
                        }
                    }
                }
            }
        }

        Some(Value::Array(subtypes))
    }

    fn handle_code_action(&self, params: &Value) -> Value {
        if self.args.no_code_actions {
            return Value::Array(Vec::new());
        }

        let context = params.get("context");
        let diagnostics = context
            .and_then(|c| c.get("diagnostics"))
            .and_then(Value::as_array);

        let mut actions = Vec::new();

        if let Some(diags) = diagnostics {
            for diag in diags {
                let source = diag.get("source").and_then(Value::as_str).unwrap_or("");
                if source == "mockls" {
                    let message = diag
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    actions.push(serde_json::json!({
                        "title": format!("fix: {message}"),
                        "kind": "quickfix",
                        "diagnostics": [diag]
                    }));

                    if self.args.multi_fix {
                        actions.push(serde_json::json!({
                            "title": format!("fix: alternative for {message}"),
                            "kind": "quickfix",
                            "diagnostics": [diag]
                        }));
                    }
                }
            }
        }

        // Always include a refactor action to verify Catenary filters it out
        actions.push(serde_json::json!({
            "title": "refactor: extract variable",
            "kind": "refactor"
        }));

        Value::Array(actions)
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

        if query.is_empty() && self.args.no_empty_query {
            return Value::Array(Vec::new());
        }

        let mut all_symbols = Vec::new();
        for (uri, content) in &self.documents {
            for mut sym in extract_symbols(content) {
                let matches = sym
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| query.is_empty() || n.contains(query));

                if matches && let Some(range) = sym.get("range").cloned() {
                    if let Some(obj) = sym.as_object_mut() {
                        if self.args.resolve_provider {
                            // URI-only location (no range) — client must resolve
                            obj.insert("location".to_string(), serde_json::json!({ "uri": uri }));
                        } else {
                            obj.insert(
                                "location".to_string(),
                                serde_json::json!({ "uri": uri, "range": range }),
                            );
                        }
                        obj.remove("range");
                        obj.remove("selectionRange");
                    }
                    all_symbols.push(sym);
                }
            }
        }

        // Simulate truncation: cap empty-query results to --symbol-limit N
        if query.is_empty()
            && let Some(limit) = self.args.symbol_limit
        {
            all_symbols.truncate(limit);
        }

        Value::Array(all_symbols)
    }

    fn handle_workspace_symbol_resolve(&self, params: &Value) -> Option<Value> {
        let name = params.get("name").and_then(Value::as_str)?;
        let uri = params
            .get("location")
            .and_then(|loc| loc.get("uri"))
            .and_then(Value::as_str)?;

        let content = self.documents.get(uri)?;

        // Find the symbol by name in the document to get its range
        for sym in extract_symbols(content) {
            if sym.get("name").and_then(Value::as_str) == Some(name) {
                let range = sym.get("range")?;
                let mut resolved = params.clone();
                if let Some(obj) = resolved.as_object_mut() {
                    obj.insert(
                        "location".to_string(),
                        serde_json::json!({ "uri": uri, "range": range }),
                    );
                }
                return Some(resolved);
            }
        }

        // Symbol not found — return as-is
        Some(params.clone())
    }

    fn publish_diagnostics(&self, uri: &str) {
        let delay = self.args.diagnostics_delay;
        let uri_owned = uri.to_string();
        let writer = self.writer.clone();
        let publish_version = self.args.publish_version;
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };
        // Capture line count at publish time so delayed publications
        // reflect the content that triggered them, not later edits.
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());

        if delay > 0 {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                send_diagnostics_notification(&writer, &uri_owned, version, line_count);
            });
        } else {
            send_diagnostics_notification(&self.writer, &uri_owned, version, line_count);
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

    fn simulate_progress_around_diagnostics(&self, uri: &str) {
        let uri_owned = uri.to_string();
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();
        let no_diagnostics = self.args.no_diagnostics;
        let publish_version = self.args.publish_version;
        let diagnostics_delay = self.args.diagnostics_delay;
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };

        std::thread::spawn(move || {
            let token = "mockls-checking";

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
                        "value": { "kind": "begin", "title": "Checking", "percentage": 0 }
                    }
                }),
            );

            if diagnostics_delay > 0 {
                std::thread::sleep(Duration::from_millis(diagnostics_delay));
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }

            if !no_diagnostics {
                send_diagnostics_notification(&writer, &uri_owned, version, line_count);
            }

            std::thread::sleep(Duration::from_millis(50));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "end", "message": "Checking complete" }
                    }
                }),
            );
        });
    }

    /// Simulates flycheck: progress Begin → spawn subprocess → wait →
    /// publish diagnostics → progress End. Runs in a background thread
    /// so the main message loop stays responsive.
    fn run_flycheck(&self, uri: &str, command: &str) {
        let uri_owned = uri.to_string();
        let command_owned = command.to_string();
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();
        let no_diagnostics = self.args.no_diagnostics;
        let publish_version = self.args.publish_version;
        let flycheck_ticks = self.args.flycheck_ticks;
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };

        std::thread::spawn(move || {
            let token = "mockls-flycheck";

            // Create progress token
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

            // Progress Begin
            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "begin", "title": "Flycheck", "percentage": 0 }
                    }
                }),
            );

            // Spawn the flycheck subprocess and wait for it to exit.
            // This is where mockls goes to Sleeping while mockc burns CPU.
            let parts: Vec<&str> = command_owned.split_whitespace().collect();
            if let Some((program, cmd_args)) = parts.split_first() {
                let mut cmd = std::process::Command::new(program);
                cmd.args(cmd_args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if let Some(ticks) = flycheck_ticks {
                    cmd.arg("--ticks").arg(ticks.to_string());
                }
                let _ = cmd.status();
            }

            // Publish diagnostics after subprocess completes
            if !no_diagnostics {
                send_diagnostics_notification(&writer, &uri_owned, version, line_count);
            }

            std::thread::sleep(Duration::from_millis(50));

            // Progress End
            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "end", "message": "Flycheck complete" }
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
fn send_diagnostics_notification(
    writer: &Writer,
    uri: &str,
    version: Option<i32>,
    line_count: usize,
) {
    let mut params = serde_json::json!({
        "uri": uri,
        "diagnostics": [{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 2,
            "source": "mockls",
            "message": format!("mockls: mock diagnostic ({line_count} lines)")
        }]
    });

    if let Some(v) = version {
        params["version"] = serde_json::json!(v);
    }

    send_message(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": params
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

/// Extract the symbol name from a declaration line. If the position lands on
/// the keyword (e.g., `fn`, `let`), returns the name that follows it.
fn extract_symbol_name(content: &str, line: usize, col: usize) -> Option<String> {
    let word = extract_word(content, line, col)?;

    let keywords = [
        "fn",
        "function",
        "def",
        "let",
        "const",
        "var",
        "struct",
        "class",
        "enum",
        "interface",
        "trait",
        "mod",
        "module",
        "type",
        "method",
        "field",
    ];

    if keywords.contains(&word.as_str()) {
        let line_text = content.lines().nth(line)?;
        let kw_with_space = format!("{word} ");
        let kw_pos = line_text.find(&kw_with_space)?;
        let after_kw = &line_text[kw_pos + kw_with_space.len()..];
        let name: String = after_kw
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() { None } else { Some(name) }
    } else {
        Some(word)
    }
}

/// Find the nearest enclosing function for a given line by searching backwards.
fn find_enclosing_function(content: &str, target_line: usize) -> Option<(String, usize)> {
    let fn_keywords = ["fn ", "function ", "def ", "method "];

    for line_idx in (0..target_line).rev() {
        let line_text = content.lines().nth(line_idx)?;
        let trimmed = line_text.trim_start();

        for kw in &fn_keywords {
            if let Some(after_kw) = trimmed.strip_prefix(kw) {
                let name: String = after_kw
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    return Some((name, line_idx));
                }
            }
        }
    }

    None
}

const fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract type annotations from content: maps `symbol_name → type_name`.
///
/// Looks for `: TypeName` after keyword-declared symbols.
/// Example: `let count: Counter = 0` → `("count", "Counter")`.
fn extract_types(content: &str) -> HashMap<String, String> {
    let mut types = HashMap::new();
    let keywords: &[&str] = &[
        "fn ",
        "function ",
        "def ",
        "let ",
        "const ",
        "var ",
        "struct ",
        "class ",
        "enum ",
        "interface ",
        "trait ",
        "mod ",
        "module ",
        "type ",
        "method ",
        "field ",
    ];

    for line_text in content.lines() {
        let trimmed = line_text.trim_start();
        let prefix_len = keywords
            .iter()
            .find_map(|kw| trimmed.starts_with(kw).then_some(kw.len()));

        let Some(prefix_len) = prefix_len else {
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

        // Look for `: TypeName` after the name
        let after_name = &after_keyword[name.len()..];
        let Some(colon_pos) = after_name.find(": ") else {
            continue;
        };

        let after_colon = &after_name[colon_pos + 2..];
        let type_name: String = after_colon
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        if !type_name.is_empty() {
            types.insert(name, type_name);
        }
    }

    types
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
        } else if trimmed.starts_with("struct ") {
            (23, 7)
        } else if trimmed.starts_with("class ") {
            (5, 6)
        } else if trimmed.starts_with("enum ") {
            (10, 5)
        } else if trimmed.starts_with("interface ") {
            (11, 10)
        } else if trimmed.starts_with("trait ") {
            (11, 6)
        } else if trimmed.starts_with("mod ") {
            (2, 4)
        } else if trimmed.starts_with("module ") {
            (2, 7)
        } else if trimmed.starts_with("type ") {
            (26, 5)
        } else if trimmed.starts_with("method ") {
            (6, 7)
        } else if trimmed.starts_with("field ") {
            (8, 6)
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
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MOCK_LANG_A: &str = "yX4Za";

    fn default_args() -> Args {
        Args {
            name: MOCK_LANG_A.to_string(),
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
            publish_version: false,
            progress_on_change: false,
            cpu_busy: None,
            flycheck_command: None,
            advertise_save: false,
            notification_log: None,
            content_modified_once: false,
            cpu_on_workspace_change: None,
            cpu_on_initialized: None,
            log_init_params: None,
            flycheck_ticks: None,
            scan_roots: false,
            no_code_actions: false,
            multi_fix: false,
            resolve_provider: false,
            no_empty_query: false,
            symbol_limit: None,
            literal_keyword_hover: false,
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
                    "languageId": "mock",
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
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello()\necho hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover on 'echo' (regular word)
        input.extend(frame(&hover_request(2, uri, 1, 0)));
        // Hover on 'fn' keyword at (0,0) — should resolve to 'hello'
        input.extend(frame(&hover_request(3, uri, 0, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        let hover_echo = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        assert!(hover_echo["error"].is_null(), "Expected no error");
        let result = &hover_echo["result"];
        assert!(result.is_object());
        assert_eq!(result["contents"]["kind"], "markdown");
        let value = result["contents"]["value"].as_str().unwrap_or("");
        assert!(value.contains("echo"), "Expected 'echo' in hover content");

        // Hover on keyword should return symbol name, not 'fn'
        let hover_kw = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("hover response with id=3");

        assert!(hover_kw["error"].is_null(), "Expected no error");
        let kw_value = hover_kw["result"]["contents"]["value"]
            .as_str()
            .unwrap_or("");
        assert!(
            kw_value.contains("hello"),
            "Hover on keyword should contain 'hello', got: {kw_value}"
        );
    }

    #[test]
    fn test_definition_response_structure() {
        let uri = "file:///tmp/test.yX4Za";
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
        let uri = "file:///tmp/test.yX4Za";
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

    fn type_definition_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/typeDefinition",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    #[test]
    fn test_extract_symbols_all_kinds() {
        let content = "\
struct MyStruct
class MyClass
enum MyEnum
interface MyInterface
trait MyTrait
mod my_mod
module my_module
type MyType
method my_method
field my_field
fn my_func
let my_var
const MY_CONST
";
        let symbols = extract_symbols(content);
        let kinds: Vec<(String, u64)> = symbols
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().expect("name").to_string(),
                    s["kind"].as_u64().expect("kind"),
                )
            })
            .collect();

        assert!(
            kinds.contains(&("MyStruct".to_string(), 23)),
            "struct → Struct(23)"
        );
        assert!(
            kinds.contains(&("MyClass".to_string(), 5)),
            "class → Class(5)"
        );
        assert!(
            kinds.contains(&("MyEnum".to_string(), 10)),
            "enum → Enum(10)"
        );
        assert!(
            kinds.contains(&("MyInterface".to_string(), 11)),
            "interface → Interface(11)"
        );
        assert!(
            kinds.contains(&("MyTrait".to_string(), 11)),
            "trait → Interface(11)"
        );
        assert!(
            kinds.contains(&("my_mod".to_string(), 2)),
            "mod → Module(2)"
        );
        assert!(
            kinds.contains(&("my_module".to_string(), 2)),
            "module → Module(2)"
        );
        assert!(
            kinds.contains(&("MyType".to_string(), 26)),
            "type → TypeParameter(26)"
        );
        assert!(
            kinds.contains(&("my_method".to_string(), 6)),
            "method → Method(6)"
        );
        assert!(
            kinds.contains(&("my_field".to_string(), 8)),
            "field → Field(8)"
        );
        assert!(
            kinds.contains(&("my_func".to_string(), 12)),
            "fn → Function(12)"
        );
        assert!(
            kinds.contains(&("my_var".to_string(), 13)),
            "let → Variable(13)"
        );
        assert!(
            kinds.contains(&("MY_CONST".to_string(), 14)),
            "const → Constant(14)"
        );
    }

    #[test]
    fn test_type_annotations_parsed() {
        let content = "\
let x: Foo
fn bar: Result
const PI: f64
";
        let types = extract_types(content);
        assert_eq!(types.get("x").map(String::as_str), Some("Foo"));
        assert_eq!(types.get("bar").map(String::as_str), Some("Result"));
        assert_eq!(types.get("PI").map(String::as_str), Some("f64"));
    }

    #[test]
    fn test_type_definition_cross_file() {
        let uri_a = "file:///tmp/types.yX4Za";
        let text_a = "struct Foo\n";
        let uri_b = "file:///tmp/usage.yX4Za";
        let text_b = "let x: Foo\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Request typeDefinition on 'x' in uri_b (line 0, character 4)
        input.extend(frame(&type_definition_request(2, uri_b, 0, 4)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let td = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("typeDefinition response with id=2");

        assert!(td["error"].is_null(), "Expected no error");
        let result = &td["result"];
        assert_eq!(
            result["uri"], uri_a,
            "Type definition should point to the file with struct Foo"
        );
        assert_eq!(result["range"]["start"]["line"], 0);
    }

    #[test]
    fn test_definition_cross_file() {
        let uri_a = "file:///tmp/defs.yX4Za";
        let text_a = "fn helper()\n";
        let uri_b = "file:///tmp/caller.yX4Za";
        let text_b = "helper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Request definition on 'helper' in uri_b (line 0, character 0)
        input.extend(frame(&definition_request(2, uri_b, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");

        assert!(def["error"].is_null(), "Expected no error");
        let result = &def["result"];
        assert_eq!(
            result["uri"], uri_a,
            "Definition should point to the file with fn helper()"
        );
        assert_eq!(result["range"]["start"]["line"], 0);
    }

    #[test]
    fn test_hover_on_keyword_returns_symbol_name() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn callee()\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 0) — lands on the 'fn' keyword
        input.extend(frame(&hover_request(2, uri, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("callee"),
            "Hover on keyword should return 'callee', got: {value}"
        );
        assert!(
            !value.contains("```\nfn\n```"),
            "Hover should not be bare keyword 'fn', got: {value}"
        );
    }

    #[test]
    fn test_hover_on_symbol_name_returns_name() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn callee()\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 3) — lands on the 'c' in 'callee'
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("callee"),
            "Hover on symbol name should return 'callee', got: {value}"
        );
    }

    #[test]
    fn test_hover_on_struct_keyword() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "struct MyStruct\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 0) — lands on the 'struct' keyword
        input.extend(frame(&hover_request(2, uri, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("MyStruct"),
            "Hover on struct keyword should return 'MyStruct', got: {value}"
        );
        assert!(
            !value.contains("```\nstruct\n```"),
            "Hover should not be bare keyword 'struct', got: {value}"
        );
    }

    #[test]
    fn test_definition_with_imports() {
        let uri_defs = "file:///tmp/defs.yX4Za";
        let text_defs = "fn helper()\n";
        let uri_a = "file:///tmp/a.yX4Za";
        let text_a = "from defs import helper\nhelper\n";
        let uri_b = "file:///tmp/b.yX4Za";
        let text_b = "helper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_defs, text_defs)));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Definition on 'helper' in a.sh (line 1, col 0) — import should resolve to defs.sh
        input.extend(frame(&definition_request(2, uri_a, 1, 0)));
        // Definition on 'helper' in b.sh (line 0, col 0) — no import, cross-file fallback
        input.extend(frame(&definition_request(3, uri_b, 0, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        // a.sh: import resolves to defs.sh
        let def_a = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");
        assert!(def_a["error"].is_null(), "Expected no error for a.yX4Za");
        assert_eq!(
            def_a["result"]["uri"], uri_defs,
            "Import in a.yX4Za should resolve to defs.yX4Za"
        );

        // b.yX4Za: cross-file fallback also resolves to defs.yX4Za
        let def_b = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("definition response with id=3");
        assert!(def_b["error"].is_null(), "Expected no error for b.yX4Za");
        assert_eq!(
            def_b["result"]["uri"], uri_defs,
            "Fallback in b.yX4Za should resolve to defs.yX4Za"
        );
    }
}
