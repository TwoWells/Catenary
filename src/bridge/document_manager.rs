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

use anyhow::{Result, anyhow};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, Uri,
    VersionedTextDocumentIdentifier,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use tokio::fs;
use tracing::{debug, trace};

/// Tracks the state of an open document.
struct OpenDocument {
    version: i32,
    content: String,
    mtime: SystemTime,
    last_accessed: Instant,
}

/// Manages document lifecycle for the LSP server.
///
/// The LSP protocol requires documents to be explicitly opened before
/// most operations. This manager handles opening documents on first
/// access and tracking their versions.
pub struct DocumentManager {
    documents: HashMap<PathBuf, OpenDocument>,
}

impl Default for DocumentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentManager {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
        }
    }

    /// Ensures a document is open and returns the notification to send if needed.
    ///
    /// If the document is already open but the file has changed on disk,
    /// returns a `didChange` notification instead.
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
                    doc.content = content.clone();
                    doc.mtime = mtime;
                    doc.last_accessed = Instant::now();

                    debug!("Document changed on disk: {}", path.display());

                    return Ok(Some(DocumentNotification::Change(
                        DidChangeTextDocumentParams {
                            text_document: VersionedTextDocumentIdentifier {
                                uri: path_to_uri(&path)?,
                                version: doc.version,
                            },
                            content_changes: vec![TextDocumentContentChangeEvent {
                                range: None,
                                range_length: None,
                                text: content,
                            }],
                        },
                    )));
                }
            }

            doc.last_accessed = Instant::now();
            trace!("Document already open: {}", path.display());
            return Ok(None);
        }

        // Document not open - read and open it
        let content = fs::read_to_string(&path).await?;
        let uri = path_to_uri(&path)?;

        // Detect language ID from extension
        let language_id = detect_language_id(&path);

        let doc = OpenDocument {
            version: 1,
            content: content.clone(),
            mtime,
            last_accessed: Instant::now(),
        };

        self.documents.insert(path.clone(), doc);
        debug!("Opening document: {} ({})", path.display(), language_id);

        Ok(Some(DocumentNotification::Open(
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: language_id.to_string(),
                    version: 1,
                    text: content,
                },
            },
        )))
    }

    /// Marks a document as closed and returns the notification to send.
    pub fn close(&mut self, path: &Path) -> Result<Option<DidCloseTextDocumentParams>> {
        let path = path.canonicalize()?;

        if self.documents.remove(&path).is_some() {
            debug!("Closing document: {}", path.display());
            Ok(Some(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: path_to_uri(&path)?,
                },
            }))
        } else {
            Ok(None)
        }
    }

    /// Returns paths of documents that haven't been accessed within the timeout.
    pub fn stale_documents(&self, timeout_secs: u64) -> Vec<PathBuf> {
        let now = Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        self.documents
            .iter()
            .filter_map(|(path, doc)| {
                if now.duration_since(doc.last_accessed) >= timeout {
                    Some(path.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns the URI for an open document.
    pub fn uri_for_path(&self, path: &Path) -> Result<Uri> {
        path_to_uri(&path.canonicalize()?)
    }

    /// Returns the language ID for a given path.
    pub fn language_id_for_path(&self, path: &Path) -> &'static str {
        detect_language_id(path)
    }

    /// Checks if there are any open documents for the given language ID.
    pub fn has_open_documents(&self, language_id: &str) -> bool {
        self.documents
            .keys()
            .any(|path| detect_language_id(path) == language_id)
    }
}

/// Notification to send to the LSP server.
pub enum DocumentNotification {
    Open(DidOpenTextDocumentParams),
    Change(DidChangeTextDocumentParams),
}

fn path_to_uri(path: &Path) -> Result<Uri> {
    let uri_str = format!("file://{}", path.display());
    uri_str
        .parse()
        .map_err(|e| anyhow!("Invalid path for URI: {}: {}", path.display(), e))
}

fn detect_language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("go") => "go",
        Some("py") => "python",
        Some("js") => "javascript",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("jsx") => "javascriptreact",
        Some("c") => "c",
        Some("cpp" | "cc" | "cxx") => "cpp",
        Some("h" | "hpp") => "cpp",
        Some("java") => "java",
        Some("rb") => "ruby",
        Some("sh" | "bash") => "shellscript",
        Some("zsh") => "shellscript",
        Some("json") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("toml") => "toml",
        Some("md") => "markdown",
        Some("html") => "html",
        Some("css") => "css",
        Some("lua") => "lua",
        Some("sql") => "sql",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_open_document() {
        let mut file = NamedTempFile::with_suffix(".rs").unwrap();
        writeln!(file, "fn main() {{}}").unwrap();

        let mut manager = DocumentManager::new();
        let notification = manager.ensure_open(file.path()).await.unwrap();

        assert!(notification.is_some());
        if let Some(DocumentNotification::Open(params)) = notification {
            assert_eq!(params.text_document.language_id, "rust");
            assert_eq!(params.text_document.version, 1);
            assert!(params.text_document.text.contains("fn main()"));
        } else {
            panic!("Expected Open notification");
        }
    }

    #[tokio::test]
    async fn test_already_open_no_change() {
        let mut file = NamedTempFile::with_suffix(".py").unwrap();
        writeln!(file, "print('hello')").unwrap();

        let mut manager = DocumentManager::new();

        // First open
        let notification1 = manager.ensure_open(file.path()).await.unwrap();
        assert!(notification1.is_some());

        // Second access - no notification since file unchanged
        let notification2 = manager.ensure_open(file.path()).await.unwrap();
        assert!(notification2.is_none());
    }

    #[tokio::test]
    async fn test_document_changed_on_disk() {
        let file = NamedTempFile::with_suffix(".js").unwrap();
        let path = file.path().to_path_buf();
        std::fs::write(&path, "const x = 1;").unwrap();

        let mut manager = DocumentManager::new();

        // First open
        let notification1 = manager.ensure_open(&path).await.unwrap();
        assert!(matches!(notification1, Some(DocumentNotification::Open(_))));

        // Modify file (need delay for mtime to differ on some filesystems)
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        std::fs::write(&path, "const x = 2;").unwrap();

        // Re-access - should get Change notification since content differs
        let notification2 = manager.ensure_open(&path).await.unwrap();
        assert!(
            matches!(notification2, Some(DocumentNotification::Change(_))),
            "Expected Change notification after file modification"
        );
    }

    #[tokio::test]
    async fn test_close_document() {
        let mut file = NamedTempFile::with_suffix(".go").unwrap();
        writeln!(file, "package main").unwrap();

        let mut manager = DocumentManager::new();
        manager.ensure_open(file.path()).await.unwrap();

        let close_params = manager.close(file.path()).unwrap();
        assert!(close_params.is_some());

        // Closing again should return None
        let close_params2 = manager.close(file.path()).unwrap();
        assert!(close_params2.is_none());
    }

    #[tokio::test]
    async fn test_stale_documents() {
        let mut file = NamedTempFile::with_suffix(".txt").unwrap();
        writeln!(file, "test").unwrap();

        let mut manager = DocumentManager::new();
        manager.ensure_open(file.path()).await.unwrap();

        // Wait a moment so the document becomes stale
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // With 0 second timeout, document should be stale (100ms > 0s)
        let stale = manager.stale_documents(0);
        assert_eq!(stale.len(), 1);

        // With large timeout, nothing should be stale
        let stale = manager.stale_documents(3600);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_language_detection() {
        assert_eq!(detect_language_id(Path::new("test.rs")), "rust");
        assert_eq!(detect_language_id(Path::new("test.py")), "python");
        assert_eq!(detect_language_id(Path::new("test.js")), "javascript");
        assert_eq!(detect_language_id(Path::new("test.ts")), "typescript");
        assert_eq!(detect_language_id(Path::new("test.go")), "go");
        assert_eq!(detect_language_id(Path::new("test.sh")), "shellscript");
        assert_eq!(detect_language_id(Path::new("test.bash")), "shellscript");
        assert_eq!(detect_language_id(Path::new("test.unknown")), "plaintext");
        assert_eq!(detect_language_id(Path::new("noextension")), "plaintext");
    }

    #[test]
    fn test_path_to_uri() {
        let uri = path_to_uri(Path::new("/home/user/test.rs")).unwrap();
        assert!(uri.as_str().starts_with("file:///home/user/test.rs"));
    }
}
