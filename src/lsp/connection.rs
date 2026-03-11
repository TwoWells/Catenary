// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Transport layer: process lifecycle, reader loop, request/response correlation.

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, error, trace, warn};

use super::inbox::Inbox;
use super::protocol::{self, RequestId, RequestMessage, ResponseError, ResponseMessage};

/// Poll interval for failure detection sampling.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// CPU tick threshold for request timeout: 1000 ticks = 10 CPU-seconds.
const REQUEST_THRESHOLD: u64 = 1000;

/// Owns the LSP server process, the reader loop, and request/response
/// correlation. Knows about JSON-RPC framing but nothing about LSP
/// semantics.
pub struct Connection {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
    alive: Arc<AtomicBool>,
    next_id: AtomicI64,
    inbox: Arc<dyn Inbox>,
    language: String,
    monitor: std::sync::Mutex<Option<catenary_proc::ProcessMonitor>>,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl Connection {
    /// Spawn a server process and start the reader loop.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server process cannot be spawned.
    /// - Stdin or stdout cannot be captured.
    pub fn new(
        program: &str,
        args: &[&str],
        stderr: Stdio,
        inbox: Arc<dyn Inbox>,
        language: String,
    ) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("Failed to spawn LSP server: {program}"))?;

        let monitor = child.id().and_then(catenary_proc::ProcessMonitor::new);

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("stdout not captured"))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_handle = tokio::spawn(Self::reader_loop(
            stdin.clone(),
            pending.clone(),
            alive.clone(),
            inbox.clone(),
            stdout,
        ));

        Ok(Self {
            child,
            stdin,
            pending,
            alive,
            next_id: AtomicI64::new(1),
            inbox,
            language,
            monitor: std::sync::Mutex::new(monitor),
            _reader_handle: reader_handle,
        })
    }

    /// Send a request and wait for the response with failure detection.
    ///
    /// Uses CPU-tick failure detection instead of wall-clock timeout.
    /// Falls back to a 30-second wall-clock timeout when the process
    /// monitor is unavailable (e.g., mockls in tests).
    /// Retries on `ContentModified` (-32801) or `RequestCancelled` (-32800).
    #[allow(
        clippy::too_many_lines,
        reason = "Request retry logic with failure detection"
    )]
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Retry loop for ContentModified errors
        for _attempt in 0..3 {
            let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

            let request = RequestMessage {
                jsonrpc: "2.0".to_string(),
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            };

            let (tx, rx) = oneshot::channel();
            {
                let mut pending = self.pending.lock().await;
                pending.insert(id.clone(), tx);
            }

            self.send_message(&request).await?;

            // Wait for response: select on rx + failure detection timer
            let response = {
                let mut rx = rx;
                let wall_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                let mut budget = i64::try_from(REQUEST_THRESHOLD).unwrap_or(1000);

                loop {
                    tokio::select! {
                        result = &mut rx => {
                            match result {
                                Ok(resp) => break Ok(resp),
                                Err(_) => break Err(anyhow!(
                                    "[{}] server closed connection", self.language
                                )),
                            }
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {
                            // Failure detection
                            if let Some((delta, state)) = self.sample_monitor() {
                                if state == catenary_proc::ProcessState::Dead {
                                    self.pending.lock().await.remove(&id);
                                    break Err(anyhow!(
                                        "[{}] server died during '{method}'",
                                        self.language
                                    ));
                                }
                                if state == catenary_proc::ProcessState::Running
                                    && delta > 0
                                    && !self.inbox.is_progress_active()
                                {
                                    budget -= i64::try_from(delta)
                                        .unwrap_or(budget);
                                }
                            } else if !self.is_alive() {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] server died during '{method}'",
                                    self.language
                                ));
                            }

                            if budget <= 0 {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] request '{method}' failed \
                                     (server stuck)",
                                    self.language
                                ));
                            }
                            if tokio::time::Instant::now() >= wall_deadline {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] request '{method}' timed out",
                                    self.language
                                ));
                            }
                        }
                    }
                }
            }?;

            if let Some(error) = response.error {
                // Check for ContentModified (-32801) or RequestCancelled (-32800)
                if error.code == -32801 || error.code == -32800 {
                    debug!("LSP request '{}' cancelled/modified, retrying...", method,);
                    tokio::select! {
                        () = self.inbox.state_notify().notified() => {}
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                    continue;
                }
                return Err(anyhow!(
                    "[{}] LSP error {}: {}",
                    self.language,
                    error.code,
                    error.message
                ));
            }

            return Ok(response.result.unwrap_or(serde_json::Value::Null));
        }

        Err(anyhow!(
            "[{}] request '{method}' failed after retries",
            self.language
        ))
    }

    /// Send a notification (no response expected).
    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<()> {
        let notification = super::protocol::NotificationMessage {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };

        self.send_message(&notification).await
    }

    /// Whether the server process is alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Returns a shared reference to the alive flag.
    ///
    /// Used by `TokenMonitor` which needs an `Arc<AtomicBool>` to poll
    /// server liveness without going through Connection.
    pub fn alive_flag(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    /// Sample the process monitor for CPU-tick failure detection.
    ///
    /// Returns `(delta, state)` where delta is ticks since the last sample.
    /// Returns `None` if the process is gone or monitoring is unavailable.
    pub fn sample_monitor(&self) -> Option<(u64, catenary_proc::ProcessState)> {
        self.monitor.lock().ok()?.as_mut()?.sample()
    }

    /// PID of the server process.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Send a JSON-RPC message with Content-Length header.
    async fn send_message<T: serde::Serialize + Sync>(&self, message: &T) -> Result<()> {
        let body = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        trace!("Sending LSP message: {}", body);

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        drop(stdin);

        Ok(())
    }

    /// Background task that reads LSP messages and routes them.
    #[allow(
        clippy::too_many_lines,
        reason = "Internal task requires sequential message parsing and dispatch"
    )]
    async fn reader_loop(
        stdin: Arc<Mutex<ChildStdin>>,
        pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
        alive: Arc<AtomicBool>,
        inbox: Arc<dyn Inbox>,
        stdout: tokio::process::ChildStdout,
    ) {
        let mut reader = BufReader::new(stdout);
        let mut buffer = BytesMut::with_capacity(8192);

        loop {
            // Read more data into buffer
            let mut temp = [0u8; 4096];
            match reader.read(&mut temp).await {
                Ok(0) => {
                    debug!("LSP stdout closed");
                    break;
                }
                Ok(n) => {
                    buffer.extend_from_slice(&temp[..n]);
                }
                Err(e) => {
                    error!("Error reading from LSP stdout: {}", e);
                    break;
                }
            }

            // Try to parse complete messages
            while let Ok(Some(message_str)) = protocol::try_parse_message(&mut buffer) {
                trace!("Received LSP message: {}", message_str);

                let value: serde_json::Value = match serde_json::from_str(&message_str) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse JSON: {}", e);
                        continue;
                    }
                };

                // Check message type
                if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                    // Request or Notification
                    if let Some(id) = value.get("id") {
                        // Server Request
                        debug!("Received server request: {} (id: {})", method, id);

                        let request_id =
                            serde_json::from_value(id.clone()).unwrap_or(RequestId::Number(0));

                        let params = value.get("params").unwrap_or(&serde_json::Value::Null);

                        let response = match inbox.on_request(method, params) {
                            Ok(result) => ResponseMessage {
                                jsonrpc: "2.0".to_string(),
                                id: Some(request_id),
                                result: Some(result),
                                error: None,
                            },
                            Err(e) => ResponseMessage {
                                jsonrpc: "2.0".to_string(),
                                id: Some(request_id),
                                result: None,
                                error: Some(ResponseError {
                                    code: e.code,
                                    message: e.message,
                                    data: None,
                                }),
                            },
                        };

                        if let Ok(body) = serde_json::to_string(&response) {
                            let header = format!("Content-Length: {}\r\n\r\n", body.len());
                            let mut stdin_guard = stdin.lock().await;
                            if let Err(e) = stdin_guard.write_all(header.as_bytes()).await {
                                warn!("Failed to write response header: {}", e);
                            } else if let Err(e) = stdin_guard.write_all(body.as_bytes()).await {
                                warn!("Failed to write response body: {}", e);
                            } else if let Err(e) = stdin_guard.flush().await {
                                warn!("Failed to flush response: {}", e);
                            }
                        }
                    } else {
                        // Notification
                        let params = value.get("params").unwrap_or(&serde_json::Value::Null);
                        inbox.on_notification(method, params);
                    }
                } else if value.get("id").is_some() {
                    // Response
                    if let Ok(response) = serde_json::from_value::<ResponseMessage>(value)
                        && let Some(id) = &response.id
                    {
                        let mut pending = pending.lock().await;
                        if let Some(sender) = pending.remove(id) {
                            let _ = sender.send(response);
                        } else {
                            warn!("Received response for unknown request id: {:?}", id);
                        }
                    }
                } else {
                    warn!("Unknown message format: {}", message_str);
                }
            }
        }

        // Mark server as dead and trigger inbox cleanup
        alive.store(false, Ordering::SeqCst);
        inbox.on_shutdown();
        warn!("LSP reader task exiting - server connection lost");
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // We can't await a graceful LSP shutdown here because drop is sync.
        // But we MUST ensure the child process doesn't become a zombie.
        let _ = self.child.start_kill();
    }
}
