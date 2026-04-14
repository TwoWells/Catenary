// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Multi-sink tracing dispatcher for Catenary telemetry.
//!
//! [`LoggingServer`] is a [`tracing_subscriber::Layer`] that subscribes to every
//! tracing event in the process and dispatches structured events to registered
//! sinks (notification queue, protocol-message DB, trace DB). It supports
//! two-phase construction: the Layer is installed at binary entry in a
//! `Buffering` state, and [`LoggingServer::activate`] transitions to `Active`
//! once sinks are ready, draining any buffered events through the new sinks.
//!
//! This module provides the scaffolding — types, field extraction, and the
//! Layer impl. Concrete sinks are added in subsequent tickets.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;

use chrono::DateTime;
use chrono::Utc;
use tracing::Subscriber;
use tracing_subscriber::layer::Context;

/// Severity of a logging event.
///
/// Mapped from [`tracing::Level`]. `TRACE` collapses into [`Severity::Debug`]
/// because the trace DB and notification queue don't distinguish the two.
/// Ordered so comparisons like `event.severity >= threshold` work directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Verbose diagnostic — below default notification threshold.
    Debug,
    /// Informational — below default notification threshold.
    Info,
    /// Warning — reaches default notification threshold.
    Warn,
    /// Error — reaches default notification threshold.
    Error,
}

impl Severity {
    /// Short lowercase tag used in notification rendering (`[warn]`, `[err]`).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "err",
        }
    }
}

impl From<&tracing::Level> for Severity {
    fn from(level: &tracing::Level) -> Self {
        if *level == tracing::Level::ERROR {
            Self::Error
        } else if *level == tracing::Level::WARN {
            Self::Warn
        } else if *level == tracing::Level::INFO {
            Self::Info
        } else {
            // DEBUG and TRACE collapse into Debug.
            Self::Debug
        }
    }
}

/// Structured representation of a single tracing event.
///
/// Sinks receive a `&LogEvent<'_>` after the Layer extracts fields from the
/// raw [`tracing::Event`]. Reserved fields are pulled into typed members; all
/// other structured fields land in [`LogEvent::fields`].
#[derive(Debug)]
pub struct LogEvent<'a> {
    /// Event severity (derived from tracing level).
    pub severity: Severity,
    /// `metadata().target()` — module path of the call site.
    pub target: &'a str,
    /// Rendered `message` field of the event.
    pub message: String,
    /// Protocol routing field: `"lsp"` / `"mcp"` / `"hook"` / absent.
    pub kind: Option<String>,
    /// Protocol method (e.g., `textDocument/hover`).
    pub method: Option<String>,
    /// LSP server name.
    pub server: Option<String>,
    /// Client identifier (host CLI name).
    pub client: Option<String>,
    /// In-process monotonic correlation ID.
    pub request_id: Option<i64>,
    /// Parent correlation ID (causation).
    pub parent_id: Option<i64>,
    /// Subsystem emitting the event (e.g., `"lsp.lifecycle"`).
    pub source: Option<String>,
    /// Language ID when relevant.
    pub language: Option<String>,
    /// Raw protocol JSON payload (for `kind in {lsp, mcp, hook}`).
    pub payload: Option<String>,
    /// All non-reserved structured fields.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// Owned counterpart of [`LogEvent`] for buffered replay.
///
/// Identical layout except `target` is an owned [`String`]. Used inside the
/// bootstrap buffer so events outlive the originating [`tracing::Event`].
#[derive(Debug)]
struct OwnedEvent {
    severity: Severity,
    target: String,
    message: String,
    kind: Option<String>,
    method: Option<String>,
    server: Option<String>,
    client: Option<String>,
    request_id: Option<i64>,
    parent_id: Option<i64>,
    source: Option<String>,
    language: Option<String>,
    payload: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl OwnedEvent {
    fn as_log_event(&self) -> LogEvent<'_> {
        LogEvent {
            severity: self.severity,
            target: &self.target,
            message: self.message.clone(),
            kind: self.kind.clone(),
            method: self.method.clone(),
            server: self.server.clone(),
            client: self.client.clone(),
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source.clone(),
            language: self.language.clone(),
            payload: self.payload.clone(),
            fields: self.fields.clone(),
        }
    }
}

/// Destination for dispatched logging events.
///
/// Sinks must be `Send + Sync` because tracing events fire on arbitrary
/// threads. `handle` must not block — it runs on the thread that called
/// the originating tracing macro.
pub trait Sink: Send + Sync {
    /// Handle one logging event.
    fn handle(&self, event: &LogEvent<'_>);
}

/// Queued user-facing notification.
///
/// Produced by the notification queue sink (ticket 01) and drained at
/// stationary hook points (ticket 06) into the host CLI's `systemMessage`.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Severity level.
    pub severity: Severity,
    /// Human-readable message body (without the `[severity]` prefix).
    pub message: String,
    /// When the notification was recorded.
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    /// Format the notification as `[severity] message`.
    #[must_use]
    pub fn format(&self) -> String {
        format!("[{}] {}", self.severity.tag(), self.message)
    }
}

/// Dedup key for the notification queue.
///
/// Two notifications with equal keys collapse into a single queue entry.
/// Combines identity-relevant structured fields with a normalized form of
/// the message body that strips volatile numeric suffixes.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct NotificationKey {
    /// `source` field.
    pub source: Option<String>,
    /// `server` field.
    pub server: Option<String>,
    /// `language` field.
    pub language: Option<String>,
    /// Lowercase, digits-stripped, whitespace-collapsed message body.
    pub stem: String,
}

impl NotificationKey {
    /// Build a dedup key from an event.
    #[must_use]
    pub fn from_event(event: &LogEvent<'_>) -> Self {
        Self {
            source: event.source.clone(),
            server: event.server.clone(),
            language: event.language.clone(),
            stem: normalize_stem(&event.message),
        }
    }
}

/// Normalize a message for dedup: lowercase, strip ASCII digits, collapse
/// consecutive whitespace into single spaces, and trim.
fn normalize_stem(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut prev_space = true;
    for c in message.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Default buffer capacity for bootstrap events before activation.
///
/// Sized for ~16× headroom over realistic bootstrap traffic (5–20 events
/// of config load + DB migration). At ~200–500 bytes per [`OwnedEvent`],
/// the cap bounds worst-case memory under a megabyte — small enough to
/// ignore, large enough that a runaway logging bug during bootstrap is
/// the only way to hit it.
const DEFAULT_BUFFER_CAP: usize = 4096;

/// Multi-sink tracing dispatcher. Catenary's telemetry port/adapter.
///
/// Cheaply cloneable: the public type wraps an `Arc<Inner>`, so multiple
/// handles share the same sinks and buffer. Install one clone as the
/// tracing Layer and keep another on [`crate::bridge::Toolbox`] (or
/// equivalent) for post-startup [`LoggingServer::activate`] calls.
#[derive(Clone)]
pub struct LoggingServer {
    inner: Arc<Inner>,
}

struct Inner {
    mode: Mutex<Mode>,
    // Placeholder for ticket 04: in-process monotonic correlation ID
    // counter. Kept here so the Layer's ownership stays simple.
    #[allow(
        dead_code,
        reason = "ticket 04 wires next_id() accessor; field reserved now"
    )]
    next_id: AtomicI64,
}

enum Mode {
    Buffering(BufferingState),
    Active(Vec<Arc<dyn Sink>>),
}

struct BufferingState {
    buffer: VecDeque<OwnedEvent>,
    cap: usize,
    dropped: u32,
}

impl LoggingServer {
    /// Construct in `Buffering` state with the default buffer cap (256).
    #[must_use]
    pub fn new() -> Self {
        Self::with_buffer_cap(DEFAULT_BUFFER_CAP)
    }

    /// Construct in `Buffering` state with a custom buffer cap.
    #[must_use]
    pub fn with_buffer_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode: Mutex::new(Mode::Buffering(BufferingState {
                    buffer: VecDeque::new(),
                    cap,
                    dropped: 0,
                })),
                next_id: AtomicI64::new(0),
            }),
        }
    }

    /// Transition to `Active`, draining buffered events through `sinks`
    /// in FIFO order. Subsequent events dispatch directly to `sinks`.
    /// Idempotent: calling `activate` on an already-`Active` server is a
    /// no-op.
    ///
    /// If bootstrap events were dropped due to buffer overflow, a
    /// `warn!()` is emitted after the drain describing the loss. That
    /// event flows through the now-active sinks like any other.
    pub fn activate(&self, sinks: Vec<Arc<dyn Sink>>) {
        // Snapshot for drain — one Arc-ref-count bump per sink. The
        // original `sinks` moves into `Mode::Active` below, so we need
        // a separate handle for the post-lock drain loop.
        let drain_sinks: Vec<Arc<dyn Sink>> = sinks.iter().map(Arc::clone).collect();

        let buffered = {
            let mut mode = lock_mode(&self.inner.mode);
            if matches!(&*mode, Mode::Active(_)) {
                return;
            }
            let taken = std::mem::replace(&mut *mode, Mode::Active(sinks));
            drop(mode);
            match taken {
                Mode::Buffering(bs) => bs,
                // Unreachable: we just verified the current mode was
                // Buffering while holding the lock.
                Mode::Active(_) => return,
            }
        };

        // Lock released — drain buffered events through the new sinks.
        for owned in &buffered.buffer {
            let log_event = owned.as_log_event();
            for sink in &drain_sinks {
                sink.handle(&log_event);
            }
        }

        if buffered.dropped > 0 {
            tracing::warn!(
                source = "logging.bootstrap",
                dropped = i64::from(buffered.dropped),
                "{} bootstrap events dropped (buffer overflow)",
                buffered.dropped,
            );
        }
    }

    /// Number of registered sinks. Returns 0 in `Buffering` state.
    #[must_use]
    pub fn sink_count(&self) -> usize {
        match &*lock_mode(&self.inner.mode) {
            Mode::Active(sinks) => sinks.len(),
            Mode::Buffering(_) => 0,
        }
    }

    /// Buffered event count (test/diagnostic accessor). Returns 0 in
    /// `Active` state.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        match &*lock_mode(&self.inner.mode) {
            Mode::Buffering(bs) => bs.buffer.len(),
            Mode::Active(_) => 0,
        }
    }
}

impl Default for LoggingServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Recover from Mutex poisoning by taking the inner guard.
///
/// Sinks may panic (e.g., closed DB connection). A poisoned mode Mutex must
/// still be usable so logging continues for subsequent events. The Layer
/// only mutates small state (push to `VecDeque`, swap Mode variant); any
/// partially-applied change is tolerable.
fn lock_mode(m: &Mutex<Mode>) -> std::sync::MutexGuard<'_, Mode> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for LoggingServer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let severity = Severity::from(meta.level());
        let target = meta.target();

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        // Decide under the mode lock. The Buffering arm consumes
        // `visitor` (into an OwnedEvent) and exits via `return`, so the
        // post-match path is only reached on the Active branch where
        // `visitor` is still intact — no OwnedEvent/target allocation on
        // the hot path.
        let sinks: Vec<Arc<dyn Sink>> = {
            let mut mode = lock_mode(&self.inner.mode);
            match &mut *mode {
                Mode::Active(sinks) => sinks.clone(),
                Mode::Buffering(bs) => {
                    let owned = visitor.finish_owned(severity, target.to_string());
                    if bs.buffer.len() >= bs.cap {
                        let _ = bs.buffer.pop_front();
                        bs.dropped = bs.dropped.saturating_add(1);
                    }
                    bs.buffer.push_back(owned);
                    return;
                }
            }
        };

        // Active path: borrow target directly, no allocation.
        let log_event = visitor.finish(severity, target);
        for sink in &sinks {
            sink.handle(&log_event);
        }
    }
}

/// Field extractor for tracing events.
///
/// Reserved field names (`message`, `kind`, `method`, `server`, `client`,
/// `source`, `language`, `payload`, `request_id`, `parent_id`) populate
/// typed members. All other fields collect into `fields`.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    kind: Option<String>,
    method: Option<String>,
    server: Option<String>,
    client: Option<String>,
    request_id: Option<i64>,
    parent_id: Option<i64>,
    source: Option<String>,
    language: Option<String>,
    payload: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    fn set_str(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = value,
            "kind" => self.kind = Some(value),
            "method" => self.method = Some(value),
            "server" => self.server = Some(value),
            "client" => self.client = Some(value),
            "source" => self.source = Some(value),
            "language" => self.language = Some(value),
            "payload" => self.payload = Some(value),
            _ => {
                self.fields
                    .insert(name.to_string(), serde_json::Value::String(value));
            }
        }
    }

    fn finish(self, severity: Severity, target: &str) -> LogEvent<'_> {
        LogEvent {
            severity,
            target,
            message: self.message,
            kind: self.kind,
            method: self.method,
            server: self.server,
            client: self.client,
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source,
            language: self.language,
            payload: self.payload,
            fields: self.fields,
        }
    }

    fn finish_owned(self, severity: Severity, target: String) -> OwnedEvent {
        OwnedEvent {
            severity,
            target,
            message: self.message,
            kind: self.kind,
            method: self.method,
            server: self.server,
            client: self.client,
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source,
            language: self.language,
            payload: self.payload,
            fields: self.fields,
        }
    }
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set_str(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set_str(field.name(), format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        match field.name() {
            "request_id" => self.request_id = Some(value),
            "parent_id" => self.parent_id = Some(value),
            name => {
                self.fields
                    .insert(name.to_string(), serde_json::Value::Number(value.into()));
            }
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        // request_id / parent_id accept u64 but store as i64. Values
        // exceeding i64::MAX fall through to the generic map.
        if let Ok(as_i64) = i64::try_from(value) {
            match field.name() {
                "request_id" => {
                    self.request_id = Some(as_i64);
                    return;
                }
                "parent_id" => {
                    self.parent_id = Some(as_i64);
                    return;
                }
                _ => {}
            }
        }
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::LogEvent;
    use super::LoggingServer;
    use super::Notification;
    use super::NotificationKey;
    use super::Severity;
    use super::Sink;
    use super::normalize_stem;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    /// Sink that records a configurable projection of every event it sees.
    #[derive(Default)]
    struct RecorderSink {
        events: Mutex<Vec<RecordedEvent>>,
    }

    #[derive(Debug, Clone)]
    struct RecordedEvent {
        severity: Severity,
        message: String,
        kind: Option<String>,
        method: Option<String>,
        server: Option<String>,
        client: Option<String>,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        source: Option<String>,
        language: Option<String>,
        payload: Option<String>,
        fields: serde_json::Map<String, serde_json::Value>,
    }

    impl RecorderSink {
        fn snapshot(&self) -> Vec<RecordedEvent> {
            self.events.lock().expect("lock recorder").clone()
        }
    }

    impl Sink for RecorderSink {
        fn handle(&self, event: &LogEvent<'_>) {
            self.events
                .lock()
                .expect("lock recorder")
                .push(RecordedEvent {
                    severity: event.severity,
                    message: event.message.clone(),
                    kind: event.kind.clone(),
                    method: event.method.clone(),
                    server: event.server.clone(),
                    client: event.client.clone(),
                    request_id: event.request_id,
                    parent_id: event.parent_id,
                    source: event.source.clone(),
                    language: event.language.clone(),
                    payload: event.payload.clone(),
                    fields: event.fields.clone(),
                });
        }
    }

    fn make_event(message: &str, source: Option<&str>, server: Option<&str>) -> LogEvent<'static> {
        LogEvent {
            severity: Severity::Warn,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: server.map(str::to_string),
            client: None,
            request_id: None,
            parent_id: None,
            source: source.map(str::to_string),
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        }
    }

    fn with_subscriber<F: FnOnce()>(server: LoggingServer, f: F) {
        let subscriber = tracing_subscriber::registry().with(server);
        tracing::subscriber::with_default(subscriber, f);
    }

    #[test]
    fn severity_from_level() {
        assert_eq!(Severity::from(&tracing::Level::ERROR), Severity::Error);
        assert_eq!(Severity::from(&tracing::Level::WARN), Severity::Warn);
        assert_eq!(Severity::from(&tracing::Level::INFO), Severity::Info);
        assert_eq!(Severity::from(&tracing::Level::DEBUG), Severity::Debug);
        assert_eq!(Severity::from(&tracing::Level::TRACE), Severity::Debug);
    }

    #[test]
    fn severity_ordering() {
        assert!(Severity::Error > Severity::Warn);
        assert!(Severity::Warn > Severity::Info);
        assert!(Severity::Info > Severity::Debug);
    }

    #[test]
    fn severity_tag_values() {
        assert_eq!(Severity::Debug.tag(), "debug");
        assert_eq!(Severity::Info.tag(), "info");
        assert_eq!(Severity::Warn.tag(), "warn");
        assert_eq!(Severity::Error.tag(), "err");
    }

    #[test]
    fn notification_format() {
        let n = Notification {
            severity: Severity::Warn,
            message: "rust-analyzer offline".into(),
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(n.format(), "[warn] rust-analyzer offline");
    }

    #[test]
    fn normalize_stem_strips_digits_and_collapses_whitespace() {
        assert_eq!(
            normalize_stem("Fetch Failed 42 times"),
            "fetch failed times"
        );
        assert_eq!(normalize_stem("  HI    there  "), "hi there");
        assert_eq!(normalize_stem("123"), "");
        assert_eq!(normalize_stem(""), "");
    }

    #[test]
    fn notification_key_dedups_numeric_variance() {
        let e1 = make_event(
            "rust-analyzer crashed 3 times",
            Some("lsp.lifecycle"),
            Some("rust-analyzer"),
        );
        let e2 = make_event(
            "rust-analyzer crashed 5 times",
            Some("lsp.lifecycle"),
            Some("rust-analyzer"),
        );
        assert_eq!(
            NotificationKey::from_event(&e1),
            NotificationKey::from_event(&e2)
        );
    }

    #[test]
    fn notification_key_different_server_not_deduped() {
        let e1 = make_event("server crashed", None, Some("rust-analyzer"));
        let e2 = make_event("server crashed", None, Some("pylsp"));
        assert_ne!(
            NotificationKey::from_event(&e1),
            NotificationKey::from_event(&e2)
        );
    }

    #[test]
    fn layer_in_buffering_mode_stores_events() {
        let server = LoggingServer::new();
        with_subscriber(server.clone(), || {
            tracing::warn!("first");
            tracing::info!("second");
        });
        assert_eq!(server.buffered_len(), 2);
        assert_eq!(server.sink_count(), 0);
    }

    #[test]
    fn activate_drains_buffered_events_in_order() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            tracing::warn!("first");
            tracing::warn!("second");
            tracing::warn!("third");
            server.activate(vec![sink.clone()]);
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[2].message, "third");
    }

    #[test]
    fn activate_then_live_events_dispatch_directly() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::warn!("after activate");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "after activate");
        assert_eq!(server.sink_count(), 1);
        assert_eq!(server.buffered_len(), 0);
    }

    #[test]
    fn buffer_overflow_drops_oldest_and_emits_warning() {
        let cap = 4;
        let server = LoggingServer::with_buffer_cap(cap);
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            for i in 0..10 {
                tracing::warn!("event {}", i);
            }
            server.activate(vec![sink.clone()]);
        });
        let events = sink.snapshot();
        // 4 events from the buffer (the most recent 4: 6..=9) + 1 synthetic
        // warn! emitted by activate to describe the drop.
        assert_eq!(events.len(), cap + 1);
        assert_eq!(events[0].message, "event 6");
        assert_eq!(events[1].message, "event 7");
        assert_eq!(events[2].message, "event 8");
        assert_eq!(events[3].message, "event 9");
        assert_eq!(events[cap].severity, Severity::Warn);
        assert_eq!(events[cap].source.as_deref(), Some("logging.bootstrap"));
    }

    #[test]
    fn activate_is_idempotent() {
        let server = LoggingServer::new();
        let sink_a = Arc::new(RecorderSink::default());
        let sink_b = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink_a.clone()]);
            server.activate(vec![sink_b.clone()]);
            tracing::warn!("after");
        });
        // sink_b was ignored because activate is idempotent
        assert_eq!(sink_a.snapshot().len(), 1);
        assert_eq!(sink_b.snapshot().len(), 0);
    }

    #[test]
    fn layer_dispatches_to_all_sinks_in_order() {
        let server = LoggingServer::new();
        let sink_a = Arc::new(RecorderSink::default());
        let sink_b = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink_a.clone(), sink_b.clone()]);
            tracing::warn!("one");
            tracing::info!("two");
        });
        assert_eq!(sink_a.snapshot().len(), 2);
        assert_eq!(sink_b.snapshot().len(), 2);
        assert_eq!(sink_a.snapshot()[0].message, "one");
        assert_eq!(sink_b.snapshot()[1].message, "two");
    }

    #[test]
    fn layer_extracts_all_reserved_fields() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::info!(
                kind = "lsp",
                method = "textDocument/hover",
                server = "rust-analyzer",
                client = "claude-code",
                request_id = 7_i64,
                parent_id = 3_i64,
                source = "lsp.protocol",
                language = "rust",
                payload = "{}",
                "outgoing"
            );
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind.as_deref(), Some("lsp"));
        assert_eq!(e.method.as_deref(), Some("textDocument/hover"));
        assert_eq!(e.server.as_deref(), Some("rust-analyzer"));
        assert_eq!(e.client.as_deref(), Some("claude-code"));
        assert_eq!(e.request_id, Some(7));
        assert_eq!(e.parent_id, Some(3));
        assert_eq!(e.source.as_deref(), Some("lsp.protocol"));
        assert_eq!(e.language.as_deref(), Some("rust"));
        assert_eq!(e.payload.as_deref(), Some("{}"));
        assert_eq!(e.message, "outgoing");
    }

    #[test]
    fn unreserved_fields_land_in_fields_map() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::warn!(extra = "value", count = 5_i64, ok = true, "event");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let f = &events[0].fields;
        assert_eq!(f["extra"], "value");
        assert_eq!(f["count"], 5);
        assert_eq!(f["ok"], true);
    }

    #[test]
    fn debug_formatted_value_preserved() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            let value = vec![1_u32, 2, 3];
            tracing::warn!(items = ?value, "debug-fmt");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fields["items"], "[1, 2, 3]");
    }

    #[test]
    fn concurrent_emit_during_drain() {
        // Sink with a per-handle delay to widen the drain window so the
        // concurrent thread can interleave.
        struct SlowSink {
            inner: Arc<RecorderSink>,
            delay_us: u64,
        }
        impl Sink for SlowSink {
            fn handle(&self, event: &LogEvent<'_>) {
                std::thread::sleep(std::time::Duration::from_micros(self.delay_us));
                self.inner.handle(event);
            }
        }

        let server = LoggingServer::new();
        let recorder = Arc::new(RecorderSink::default());
        let slow = Arc::new(SlowSink {
            inner: recorder.clone(),
            delay_us: 200,
        });

        // Dispatch propagates into spawned threads; `with_default` only
        // sets a thread-local subscriber for the current thread.
        let subscriber = tracing_subscriber::registry().with(server.clone());
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            // Fill buffer with 20 events before activation.
            for i in 0..20 {
                tracing::warn!(idx = i64::from(i), "buf");
            }
            let server_for_thread = server.clone();
            let dispatch_for_thread = dispatch.clone();
            let thread = std::thread::spawn(move || {
                tracing::dispatcher::with_default(&dispatch_for_thread, || {
                    // Emit while drain is happening. These see Active
                    // state and dispatch directly to sinks.
                    for i in 0..10 {
                        tracing::warn!(idx = i64::from(i), "live");
                    }
                });
                drop(server_for_thread);
            });
            server.activate(vec![slow.clone()]);
            thread.join().expect("join concurrent emitter");
            // Flush any trailing live events.
            tracing::warn!("final");
        });

        let events = recorder.snapshot();
        // Count is deterministic: 20 "buf" + 10 "live" + 1 "final" = 31.
        // No drops (well under the 4096 cap), one-way mode transition
        // means every emit path reaches a sink exactly once, and
        // thread.join() guarantees all live events fire before we check.
        let buf = events.iter().filter(|e| e.message == "buf").count();
        let live = events.iter().filter(|e| e.message == "live").count();
        let final_count = events.iter().filter(|e| e.message == "final").count();
        assert_eq!(buf, 20, "buffered events: {events:?}");
        assert_eq!(live, 10, "live events: {events:?}");
        assert_eq!(final_count, 1, "final events: {events:?}");
        assert_eq!(events.len(), 31);
    }

    #[test]
    fn sink_count_and_buffered_len_reflect_mode() {
        let server = LoggingServer::new();
        assert_eq!(server.sink_count(), 0);
        assert_eq!(server.buffered_len(), 0);
        with_subscriber(server.clone(), || {
            tracing::warn!("buffered");
            assert_eq!(server.buffered_len(), 1);
            let sink = Arc::new(RecorderSink::default());
            server.activate(vec![sink]);
            assert_eq!(server.sink_count(), 1);
            assert_eq!(server.buffered_len(), 0);
        });
    }
}
