//! In-memory LRU cache for idempotent tool responses.
//!
//! Why: many child-server calls during a session are pure reads (read_file, list_dir, get_*).
//! Re-spawning the child + re-running the call costs latency + tokens for identical data.
//!
//! Strategy:
//! - Cache only tools whose name matches an idempotency heuristic (read/list/get/search/find/...).
//! - Key = (server, tool, sha256(args)). TTL configurable (default 60s).
//! - LRU eviction at `max_entries` to bound memory.
//! - Caller can force a fresh call with `bypass_cache: true` in mcphub_call args.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_TTL_SECS: u64 = 60;
const DEFAULT_MAX_ENTRIES: usize = 256;

/// Tools matching ANY of these prefixes are considered safe to cache.
/// Conservative on purpose: better to skip a cacheable read than to serve stale state.
const IDEMPOTENT_PREFIXES: &[&str] = &[
    "read_", "read-", "list_", "list-", "get_", "get-", "fetch_", "fetch-", "search_", "search-",
    "find_", "find-", "info_", "info-", "show_", "show-", "view_", "view-", "describe_",
    "describe-", "inspect_", "inspect-", "head_", "head-", "tail_", "tail-", "count_", "count-",
    "exists", "has_", "has-", "lookup_", "lookup-", "preview_", "preview-", "render_", "render-",
];

/// Tools matching ANY of these patterns are NEVER cached, even if they happen to start with a
/// read prefix (e.g. `read_then_modify`). Mutating ops must always hit the live server.
const MUTATING_PATTERNS: &[&str] = &[
    "write", "delete", "remove", "create", "update", "set_", "set-", "apply", "execute", "exec_",
    "commit", "push", "add_", "add-", "modify", "patch", "edit", "drop", "insert", "upload",
    "send_", "send-", "post_", "post-", "put_", "put-", "publish", "deploy", "kill", "restart",
    "stop", "start_", "start-", "spawn", "fork", "trigger", "invoke_", "invoke-",
];

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub value: Value,
    pub stored_at: Instant,
    pub age_ms: u64,
}

struct Entry {
    value: Value,
    stored_at: Instant,
    last_accessed: Instant,
    hits: u64,
}

pub struct ResponseCache {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    max_entries: usize,
}

impl ResponseCache {
    pub fn new() -> Self {
        Self::with_config(DEFAULT_TTL_SECS, DEFAULT_MAX_ENTRIES)
    }

    pub fn with_config(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::with_capacity(max_entries.min(64))),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
        }
    }

    /// Returns true if this (server, tool) pair is safe to cache.
    /// Strictly: idempotent prefix AND no mutating pattern anywhere in the name.
    pub fn is_cacheable(tool: &str) -> bool {
        let lower = tool.to_lowercase();
        if MUTATING_PATTERNS.iter().any(|p| lower.contains(p)) {
            return false;
        }
        IDEMPOTENT_PREFIXES.iter().any(|p| lower.starts_with(p))
    }

    fn make_key(server: &str, tool: &str, args: &Value) -> String {
        // DefaultHasher (SipHash) is plenty for cache keying — not cryptographic.
        let args_canonical =
            serde_json::to_string(args).unwrap_or_else(|_| "null".to_string());
        let mut hasher = DefaultHasher::new();
        server.hash(&mut hasher);
        tool.hash(&mut hasher);
        args_canonical.hash(&mut hasher);
        format!("{}__{}__{:016x}", server, tool, hasher.finish())
    }

    pub fn get(&self, server: &str, tool: &str, args: &Value) -> Option<CachedResponse> {
        let key = Self::make_key(server, tool, args);
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.get_mut(&key)?;
        let age = entry.stored_at.elapsed();
        if age > self.ttl {
            entries.remove(&key);
            return None;
        }
        entry.last_accessed = Instant::now();
        entry.hits += 1;
        Some(CachedResponse {
            value: entry.value.clone(),
            stored_at: entry.stored_at,
            age_ms: age.as_millis() as u64,
        })
    }

    pub fn put(&self, server: &str, tool: &str, args: &Value, value: Value) {
        let key = Self::make_key(server, tool, args);
        let now = Instant::now();
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };

        if entries.len() >= self.max_entries {
            // Evict the least recently accessed entry to bound memory.
            if let Some(victim) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&victim);
            }
        }

        entries.insert(
            key,
            Entry {
                value,
                stored_at: now,
                last_accessed: now,
                hits: 0,
            },
        );
    }

    /// Drop expired entries. Cheap O(n), call periodically or before stats.
    pub fn purge_expired(&self) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let ttl = self.ttl;
        entries.retain(|_, e| e.stored_at.elapsed() <= ttl);
    }

    pub fn stats(&self) -> CacheStats {
        let Ok(entries) = self.entries.lock() else {
            return CacheStats::default();
        };
        let total_hits: u64 = entries.values().map(|e| e.hits).sum();
        CacheStats {
            entry_count: entries.len(),
            total_hits,
            ttl_secs: self.ttl.as_secs(),
            max_entries: self.max_entries,
        }
    }
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CacheStats {
    pub entry_count: usize,
    pub total_hits: u64,
    pub ttl_secs: u64,
    pub max_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cacheable_classifies_correctly() {
        assert!(ResponseCache::is_cacheable("read_file"));
        assert!(ResponseCache::is_cacheable("list_directory"));
        assert!(ResponseCache::is_cacheable("get_user"));
        assert!(ResponseCache::is_cacheable("search_memory"));

        assert!(!ResponseCache::is_cacheable("write_file"));
        assert!(!ResponseCache::is_cacheable("delete_branch"));
        assert!(!ResponseCache::is_cacheable("execute_sql"));
        assert!(!ResponseCache::is_cacheable("apply_migration"));
        assert!(!ResponseCache::is_cacheable("commit_changes"));
        // Mutating-name override even when prefix looks safe.
        assert!(!ResponseCache::is_cacheable("read_then_modify"));
    }

    #[test]
    fn put_get_roundtrip() {
        let cache = ResponseCache::with_config(60, 16);
        let args = json!({"path": "/tmp/x"});
        cache.put("filesystem", "read_file", &args, json!({"content": "hello"}));
        let hit = cache.get("filesystem", "read_file", &args).unwrap();
        assert_eq!(hit.value, json!({"content": "hello"}));
    }

    #[test]
    fn different_args_produce_different_keys() {
        let cache = ResponseCache::with_config(60, 16);
        cache.put("fs", "read_file", &json!({"path": "a"}), json!(1));
        cache.put("fs", "read_file", &json!({"path": "b"}), json!(2));
        assert_eq!(
            cache.get("fs", "read_file", &json!({"path": "a"})).unwrap().value,
            json!(1)
        );
        assert_eq!(
            cache.get("fs", "read_file", &json!({"path": "b"})).unwrap().value,
            json!(2)
        );
    }

    #[test]
    fn lru_evicts_oldest() {
        let cache = ResponseCache::with_config(60, 2);
        cache.put("s", "read_a", &json!({}), json!("a"));
        std::thread::sleep(Duration::from_millis(5));
        cache.put("s", "read_b", &json!({}), json!("b"));
        std::thread::sleep(Duration::from_millis(5));
        // Touch read_a so read_b becomes the LRU
        let _ = cache.get("s", "read_a", &json!({}));
        cache.put("s", "read_c", &json!({}), json!("c"));
        assert!(cache.get("s", "read_a", &json!({})).is_some());
        assert!(cache.get("s", "read_b", &json!({})).is_none());
        assert!(cache.get("s", "read_c", &json!({})).is_some());
    }
}
