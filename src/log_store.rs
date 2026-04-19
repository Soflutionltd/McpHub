//! In-memory ring buffer for MCP child server logs (`notifications/message`).
//!
//! Each entry captures a single log emitted by a child MCP server (level, message,
//! optional logger/data) along with the originating server name and a timestamp.
//! Subscribers (e.g. SSE handlers in the dashboard) get a broadcast channel so they
//! can stream new entries in real time without polling.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{broadcast, Mutex};

/// Maximum number of entries kept in memory. Oldest entries are evicted FIFO.
const MAX_ENTRIES: usize = 1000;

/// Capacity of the broadcast channel. Lagging subscribers will drop messages.
const BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Monotonic id starting at 1. Useful for "give me everything after id=N" pagination.
    pub id: u64,
    /// Unix timestamp in milliseconds.
    pub ts_ms: u64,
    /// MCP server that emitted the log.
    pub server: String,
    /// MCP log level (`debug`, `info`, `notice`, `warning`, `error`, `critical`, `alert`, `emergency`).
    /// Free-form, lower-cased.
    pub level: String,
    /// Optional logger name (some servers tag their logs).
    pub logger: Option<String>,
    /// Human readable message. May contain JSON if `data` was an object.
    pub message: String,
}

impl LogEntry {
    fn matches_level(&self, min_level: &str) -> bool {
        level_rank(&self.level) >= level_rank(min_level)
    }
}

/// Returns true when `entry_level` is at or above `min_level` in severity.
pub fn level_passes(entry_level: &str, min_level: &str) -> bool {
    level_rank(entry_level) >= level_rank(min_level)
}

fn level_rank(level: &str) -> u8 {
    match level.to_lowercase().as_str() {
        "debug" => 0,
        "info" | "notice" => 1,
        "warning" | "warn" => 2,
        "error" => 3,
        "critical" | "alert" | "emergency" | "fatal" => 4,
        _ => 1,
    }
}

#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Mutex<RingState>>,
    sender: broadcast::Sender<LogEntry>,
}

struct RingState {
    entries: VecDeque<LogEntry>,
    next_id: u64,
}

impl LogStore {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(RingState {
                entries: VecDeque::with_capacity(MAX_ENTRIES),
                next_id: 1,
            })),
            sender,
        }
    }

    /// Push a new log entry. Broadcast errors (no subscribers) are silently ignored.
    pub async fn push(&self, server: &str, level: &str, logger: Option<String>, message: String) {
        let mut state = self.inner.lock().await;
        let id = state.next_id;
        state.next_id += 1;

        let entry = LogEntry {
            id,
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            server: server.to_string(),
            level: level.to_lowercase(),
            logger,
            message,
        };

        if state.entries.len() == MAX_ENTRIES {
            state.entries.pop_front();
        }
        state.entries.push_back(entry.clone());
        drop(state);

        let _ = self.sender.send(entry);
    }

    /// Subscribe to live entries. Each subscriber sees only entries pushed AFTER it subscribes.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.sender.subscribe()
    }

    /// Snapshot of recent entries with optional filters (newest first).
    pub async fn recent(
        &self,
        limit: usize,
        server_filter: Option<&str>,
        min_level: Option<&str>,
        since_id: Option<u64>,
    ) -> Vec<LogEntry> {
        let state = self.inner.lock().await;
        let limit = limit.clamp(1, MAX_ENTRIES);
        state
            .entries
            .iter()
            .rev()
            .filter(|e| since_id.map(|s| e.id > s).unwrap_or(true))
            .filter(|e| {
                server_filter
                    .map(|s| !s.is_empty() && e.server.eq_ignore_ascii_case(s))
                    .unwrap_or(true)
            })
            .filter(|e| min_level.map(|m| e.matches_level(m)).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Total number of entries currently stored.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_push_and_recent_basic() {
        let store = LogStore::new();
        store.push("srv1", "info", None, "hello".into()).await;
        store.push("srv1", "warning", None, "watch out".into()).await;
        store.push("srv2", "error", None, "boom".into()).await;

        let all = store.recent(10, None, None, None).await;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].message, "boom");
        assert_eq!(all[2].message, "hello");
    }

    #[tokio::test]
    async fn test_filters() {
        let store = LogStore::new();
        store.push("srv1", "info", None, "a".into()).await;
        store.push("srv2", "warning", None, "b".into()).await;
        store.push("srv1", "error", None, "c".into()).await;

        let only_srv1 = store.recent(10, Some("srv1"), None, None).await;
        assert_eq!(only_srv1.len(), 2);
        assert!(only_srv1.iter().all(|e| e.server == "srv1"));

        let warn_or_more = store.recent(10, None, Some("warning"), None).await;
        assert_eq!(warn_or_more.len(), 2);
        assert!(warn_or_more.iter().all(|e| e.level == "warning" || e.level == "error"));
    }

    #[tokio::test]
    async fn test_since_id_pagination() {
        let store = LogStore::new();
        for i in 0..5 {
            store.push("srv", "info", None, format!("msg-{}", i)).await;
        }
        let after_2 = store.recent(10, None, None, Some(2)).await;
        assert_eq!(after_2.len(), 3);
        assert!(after_2.iter().all(|e| e.id > 2));
    }

    #[tokio::test]
    async fn test_ring_buffer_eviction() {
        let store = LogStore::new();
        for i in 0..(MAX_ENTRIES + 10) {
            store.push("srv", "info", None, format!("msg-{}", i)).await;
        }
        assert_eq!(store.len().await, MAX_ENTRIES);
        let recent = store.recent(1, None, None, None).await;
        assert_eq!(recent[0].message, format!("msg-{}", MAX_ENTRIES + 9));
    }

    #[tokio::test]
    async fn test_subscribe_receives_new_entries() {
        let store = LogStore::new();
        let mut rx = store.subscribe();
        store.push("srv", "info", None, "first".into()).await;
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(received.message, "first");
    }
}
