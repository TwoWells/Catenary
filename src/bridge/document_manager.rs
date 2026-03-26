// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::fs;
use tracing::{debug, trace};

use super::filesystem_manager::detect_language_id_opt;
use crate::db;

/// Tracks the state of an open document.
struct OpenDocument {
    version: i32,
    content: String,
    mtime: SystemTime,
}

/// Manages document lifecycle for the LSP server.
///
/// The LSP protocol requires documents to be explicitly opened before
/// most operations. This manager handles opening documents on first
/// access, tracking their versions, and detecting changes on disk.
///
/// Also owns the editing mode lifecycle (`start_editing` / `finish_editing`).
/// In v1, these are thin `SQLite` wrappers. In waitv2, they will manage
/// `didOpen`/`didChange`/`didSave`/`didClose` directly.
pub struct DocumentManager {
    documents: HashMap<PathBuf, OpenDocument>,
    session_id: String,
}

impl DocumentManager {
    /// Creates a new, empty `DocumentManager` for the given session.
    #[must_use]
    pub fn new(session_id: String) -> Self {
        Self {
            documents: HashMap::new(),
            session_id,
        }
    }

    /// Ensures a document is open and returns the notification to send if needed.
    ///
    /// If the document is already open but the file has changed on disk,
    /// returns a `didChange` notification instead.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path cannot be canonicalized.
    /// - File metadata cannot be read.
    /// - The file cannot be read from disk.
    /// - The path cannot be converted to a valid URI.
    pub async fn ensure_open(&mut self, path: &Path) -> Result<Option<DocumentNotification>> {
        let path = path.canonicalize()?;
        let metadata = fs::metadata(&path).await?;
        let mtime = metadata.modified()?;

        if let Some(doc) = self.documents.get_mut(&path) {
            // Document already open - check if it changed on disk
            if mtime > doc.mtime {
                let content = fs::read_to_string(&path).await?;
                if content != doc.content {
                    doc.version += 1;
                    doc.content.clone_from(&content);
                    doc.mtime = mtime;

                    debug!("Document changed on disk: {}", path.display());

                    return Ok(Some(DocumentNotification::Change {
                        uri: path_to_uri(&path),
                        version: doc.version,
                        text: content,
                    }));
                }
            }

            trace!("Document already open: {}", path.display());
            return Ok(None);
        }

        // Document not open - read and open it
        let content = fs::read_to_string(&path).await?;
        let uri = path_to_uri(&path);

        // Detect language ID from extension
        let language_id = detect_language_id_opt(&path).unwrap_or("plaintext");

        let doc = OpenDocument {
            version: 1,
            content: content.clone(),
            mtime,
        };

        self.documents.insert(path.clone(), doc);
        debug!("Opening document: {} ({})", path.display(), language_id);

        Ok(Some(DocumentNotification::Open {
            uri,
            language_id: language_id.to_string(),
            version: 1,
            text: content,
        }))
    }

    /// Marks a document as closed and returns the notification to send.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be canonicalized or converted to a URI.
    pub fn close(&mut self, path: &Path) -> Result<Option<String>> {
        let path = path.canonicalize()?;

        if self.documents.remove(&path).is_some() {
            debug!("Closing document: {}", path.display());
            Ok(Some(path_to_uri(&path)))
        } else {
            Ok(None)
        }
    }

    /// Returns the URI for an open document.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be canonicalized or converted to a URI.
    pub fn uri_for_path(&self, path: &Path) -> Result<String> {
        Ok(path_to_uri(&path.canonicalize()?))
    }

    /// Notifies the manager that a file was written externally (by Catenary itself).
    ///
    /// Updates internal state with the new content and returns the appropriate
    /// LSP notification to send, without re-reading from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be canonicalized or converted to a URI.
    pub fn notify_external_write(
        &mut self,
        path: &Path,
        content: &str,
        mtime: SystemTime,
    ) -> Result<DocumentNotification> {
        let path = path.canonicalize()?;
        let uri = path_to_uri(&path);

        if let Some(doc) = self.documents.get_mut(&path) {
            // Already open — send didChange
            doc.version += 1;
            doc.content = content.to_string();
            doc.mtime = mtime;
            debug!("External write (change): {}", path.display());

            Ok(DocumentNotification::Change {
                uri,
                version: doc.version,
                text: content.to_string(),
            })
        } else {
            // Not open — send didOpen
            let language_id = detect_language_id_opt(&path).unwrap_or("plaintext");

            let doc = OpenDocument {
                version: 1,
                content: content.to_string(),
                mtime,
            };

            self.documents.insert(path.clone(), doc);
            debug!(
                "External write (open): {} ({})",
                path.display(),
                language_id
            );

            Ok(DocumentNotification::Open {
                uri,
                language_id: language_id.to_string(),
                version: 1,
                text: content.to_string(),
            })
        }
    }
    // ── Editing mode lifecycle ────────────────────────────────────────────

    /// Enters editing mode. Diagnostics are suppressed until
    /// [`finish_editing`](Self::finish_editing) is called.
    ///
    /// Returns `Ok(true)` if editing mode was entered, `Ok(false)` if
    /// already in editing mode (no-op).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn start_editing(&self, agent_id: &str) -> Result<bool> {
        let conn = db::open()?;
        db::start_editing(&conn, &self.session_id, agent_id)
    }

    /// Exits editing mode and returns accumulated file paths. The caller
    /// is responsible for running diagnostics on the returned files.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not in editing mode or a database
    /// operation fails.
    pub fn finish_editing(&self, agent_id: &str) -> Result<Vec<String>> {
        let conn = db::open()?;
        let files = db::drain_editing_files(&conn, &self.session_id, agent_id)?;
        db::done_editing(&conn, &self.session_id, agent_id)?;
        Ok(files)
    }

    /// Checks if an agent is in editing mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn is_agent_editing(&self, agent_id: &str) -> Result<bool> {
        let conn = db::open()?;
        db::is_agent_editing(&conn, &self.session_id, agent_id)
    }

    /// Accumulates a modified file path during editing mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn add_editing_file(&self, agent_id: &str, file_path: &str) -> Result<()> {
        let conn = db::open()?;
        db::add_editing_file(&conn, &self.session_id, agent_id, file_path)
    }
}

/// Notification to send to the LSP server.
pub enum DocumentNotification {
    /// A `textDocument/didOpen` notification.
    Open {
        /// Document URI (`file://` scheme).
        uri: String,
        /// Language identifier (e.g. `"rust"`, `"python"`).
        language_id: String,
        /// Document version.
        version: i32,
        /// Full document text.
        text: String,
    },
    /// A `textDocument/didChange` notification.
    Change {
        /// Document URI (`file://` scheme).
        uri: String,
        /// Document version.
        version: i32,
        /// Full document text.
        text: String,
    },
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_open_document() -> Result<()> {
        let mut file = NamedTempFile::with_suffix(".rs")?;
        writeln!(file, "fn main() {{}}")?;

        let mut manager = DocumentManager::new(String::new());
        let notification = manager.ensure_open(file.path()).await?;

        assert!(notification.is_some());
        if let Some(DocumentNotification::Open {
            language_id,
            version,
            text,
            ..
        }) = notification
        {
            assert_eq!(language_id, "rust");
            assert_eq!(version, 1);
            assert!(text.contains("fn main()"));
        } else {
            anyhow::bail!("Expected Open notification");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_already_open_no_change() -> Result<()> {
        let mut file = NamedTempFile::with_suffix(".py")?;
        writeln!(file, "print('hello')")?;

        let mut manager = DocumentManager::new(String::new());

        // First open
        let notification1 = manager.ensure_open(file.path()).await?;
        assert!(notification1.is_some());

        // Second access - no notification since file unchanged
        let notification2 = manager.ensure_open(file.path()).await?;
        assert!(notification2.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_document_changed_on_disk() -> Result<()> {
        let file = NamedTempFile::with_suffix(".js")?;
        let path = file.path().to_path_buf();
        std::fs::write(&path, "const x = 1;")?;

        let mut manager = DocumentManager::new(String::new());

        // First open
        let notification1 = manager.ensure_open(&path).await?;
        assert!(matches!(
            notification1,
            Some(DocumentNotification::Open { .. })
        ));

        // Modify file (need delay for mtime to differ on some filesystems)
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        std::fs::write(&path, "const x = 2;")?;

        // Re-access - should get Change notification since content differs
        let notification2 = manager.ensure_open(&path).await?;
        assert!(
            matches!(notification2, Some(DocumentNotification::Change { .. })),
            "Expected Change notification after file modification"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_close_document() -> Result<()> {
        let mut file = NamedTempFile::with_suffix(".go")?;
        writeln!(file, "package main")?;

        let mut manager = DocumentManager::new(String::new());
        manager.ensure_open(file.path()).await?;

        let close_params = manager.close(file.path())?;
        assert!(close_params.is_some());

        // Closing again should return None
        let close_params2 = manager.close(file.path())?;
        assert!(close_params2.is_none());
        Ok(())
    }

    #[test]
    fn test_path_to_uri() {
        let uri = path_to_uri(Path::new("/home/user/test.rs"));
        assert!(uri.starts_with("file:///home/user/test.rs"));
    }
}
