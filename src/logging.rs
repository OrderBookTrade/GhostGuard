//! JSONL audit log writer.
//!
//! Append-only, one JSON object per line, flushed after every write.
//! Intended for durable forensic trails of verdicts and predictive warnings.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

/// Thread-safe JSONL appender.
///
/// Writes are serialized through a `Mutex<BufWriter<File>>` and flushed
/// after every line so that crashes do not lose recent entries.
pub struct JsonlWriter {
    path: PathBuf,
    inner: Mutex<BufWriter<tokio::fs::File>>,
}

impl JsonlWriter {
    /// Open a JSONL file for appending. Creates the parent directory if needed.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent)
                    .await
                    .with_context(|| format!("create_dir_all {parent:?}"))?;
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("open {path:?}"))?;

        Ok(Self {
            path,
            inner: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Optional constructor — returns None when `path` is empty.
    /// Useful so callers can conditionally disable logging via config.
    pub async fn maybe_open(path: &str) -> Result<Option<Arc<JsonlWriter>>> {
        if path.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self::open(path).await?)))
    }

    /// Serialize `entry` and append it as a single line. Flushes on every call.
    pub async fn append<T: Serialize>(&self, entry: &T) -> Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');

        let mut guard = self.inner.lock().await;
        guard.write_all(&line).await?;
        guard.flush().await?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tempfile::tempdir;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Entry {
        id: u64,
        msg: String,
    }

    #[tokio::test]
    async fn test_jsonl_append_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub/out.jsonl");

        let writer = JsonlWriter::open(&path).await.unwrap();
        writer
            .append(&Entry {
                id: 1,
                msg: "one".into(),
            })
            .await
            .unwrap();
        writer
            .append(&Entry {
                id: 2,
                msg: "two".into(),
            })
            .await
            .unwrap();
        drop(writer);

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let e0: Entry = serde_json::from_str(lines[0]).unwrap();
        let e1: Entry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(
            e0,
            Entry {
                id: 1,
                msg: "one".into()
            }
        );
        assert_eq!(
            e1,
            Entry {
                id: 2,
                msg: "two".into()
            }
        );
    }

    #[tokio::test]
    async fn test_maybe_open_empty_path() {
        let result = JsonlWriter::maybe_open("").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_maybe_open_real_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let result = JsonlWriter::maybe_open(path.to_str().unwrap())
            .await
            .unwrap();
        assert!(result.is_some());
    }
}
