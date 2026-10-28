//! In-memory LRU cache for idempotent tool responses.
//!
//! Why: many child-server calls during a session are pure reads (read_file, list_dir, get_*).
//! Re-spawning the child + re-running the call costs latency + tokens for identical data.
//!
//! Strategy:
//! - Cache only tools whose name matches an idempotency heuristic (read/list/get/search/find/...).
//! - Key = (server, tool, hash(args)). TTL configurable (default 60s).
//! - Never cache an error result (`isError`): a transient failure must not be replayed.
//! - Any non-cacheable call on a server drops that server's entries: a write followed by a read
//!   (add_memory then search_memory) must see the write.
//! - Entries carry the child manager's epoch: a server restarted, lost or reconfigured since the
//!   entry was stored is never served from cache.
//! - Bounded by entry count, by size per entry and by total size (LRU eviction).
//! - Caller can force a fresh call with `bypass_cache: true` in mcphub_call args.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_TTL_SECS: u64 = 60;
const DEFAULT_MAX_ENTRIES: usize = 256;
const DEFAULT_MAX_ENTRY_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

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
    pub age_ms: u64,
}

struct Entry {
    server: String,
    value: Value,
    bytes: usize,
    epoch: u64,
    stored_at: Instant,
    last_accessed: Instant,
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    total_bytes: usize,
}

impl Store {
    fn remove(&mut self, key: &str) {
        if let Some(e) = self.entries.remove(key) {
            self.total_bytes -= e.bytes;
        }
    }

    fn retain(&mut self, mut keep: impl FnMut(&Entry) -> bool) {
        let mut freed = 0;
        self.entries.retain(|_, e| {
            let k = keep(e);
            if !k {
                freed += e.bytes;
            }
            k
        });
        self.total_bytes -= freed;
    }
}

pub struct ResponseCache {
    store: Mutex<Store>,
    ttl: Duration,
    max_entries: usize,
    max_entry_bytes: usize,
    max_total_bytes: usize,
}

impl ResponseCache {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_TTL_SECS,
            DEFAULT_MAX_ENTRIES,
            DEFAULT_MAX_ENTRY_BYTES,
            DEFAULT_MAX_TOTAL_BYTES,
        )
    }

    pub fn with_limits(
        ttl_secs: u64,
        max_entries: usize,
        max_entry_bytes: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            store: Mutex::new(Store::default()),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            max_entry_bytes,
            max_total_bytes,
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

    pub fn get(&self, server: &str, tool: &str, args: &Value, epoch: u64) -> Option<CachedResponse> {
        let key = Self::make_key(server, tool, args);
        let mut store = self.store.lock().ok()?;
        let entry = store.entries.get_mut(&key)?;
        let age = entry.stored_at.elapsed();
        if age > self.ttl || entry.epoch != epoch {
            store.remove(&key);
            return None;
        }
        entry.last_accessed = Instant::now();
        Some(CachedResponse {
            value: entry.value.clone(),
            age_ms: age.as_millis() as u64,
        })
    }

    pub fn put(&self, server: &str, tool: &str, args: &Value, value: Value, epoch: u64) {
        if value.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return;
        }
        let bytes = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(usize::MAX);
        if bytes > self.max_entry_bytes {
            return;
        }
        let key = Self::make_key(server, tool, args);
        let now = Instant::now();
        let Ok(mut store) = self.store.lock() else {
            return;
        };
        store.remove(&key);
        let ttl = self.ttl;
        store.retain(|e| e.stored_at.elapsed() <= ttl && e.epoch == epoch);
        while !store.entries.is_empty()
            && (store.entries.len() >= self.max_entries
                || store.total_bytes + bytes > self.max_total_bytes)
        {
            let Some(victim) = store
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            store.remove(&victim);
        }
        store.total_bytes += bytes;
        store.entries.insert(
            key,
            Entry {
                server: server.to_string(),
                value,
                bytes,
                epoch,
                stored_at: now,
                last_accessed: now,
            },
        );
    }

    /// After a call that may have changed the server's state.
    pub fn invalidate_server(&self, server: &str) {
        if let Ok(mut store) = self.store.lock() {
            store.retain(|e| e.server != server);
        }
    }
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn small(max_entries: usize) -> ResponseCache {
        ResponseCache::with_limits(60, max_entries, 1024, 1024 * 1024)
    }

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
        let cache = small(16);
        let args = json!({"path": "/tmp/x"});
        cache.put("filesystem", "read_file", &args, json!({"content": "hello"}), 0);
        let hit = cache.get("filesystem", "read_file", &args, 0).unwrap();
        assert_eq!(hit.value, json!({"content": "hello"}));
    }

    #[test]
    fn different_args_produce_different_keys() {
        let cache = small(16);
        cache.put("fs", "read_file", &json!({"path": "a"}), json!(1), 0);
        cache.put("fs", "read_file", &json!({"path": "b"}), json!(2), 0);
        assert_eq!(cache.get("fs", "read_file", &json!({"path": "a"}), 0).unwrap().value, json!(1));
        assert_eq!(cache.get("fs", "read_file", &json!({"path": "b"}), 0).unwrap().value, json!(2));
    }

    #[test]
    fn lru_evicts_oldest() {
        let cache = small(2);
        cache.put("s", "read_a", &json!({}), json!("a"), 0);
        std::thread::sleep(Duration::from_millis(5));
        cache.put("s", "read_b", &json!({}), json!("b"), 0);
        std::thread::sleep(Duration::from_millis(5));
        // Touch read_a so read_b becomes the LRU
        let _ = cache.get("s", "read_a", &json!({}), 0);
        cache.put("s", "read_c", &json!({}), json!("c"), 0);
        assert!(cache.get("s", "read_a", &json!({}), 0).is_some());
        assert!(cache.get("s", "read_b", &json!({}), 0).is_none());
        assert!(cache.get("s", "read_c", &json!({}), 0).is_some());
    }

    #[test]
    fn errors_are_never_cached() {
        let cache = small(16);
        cache.put("s", "get_x", &json!({}), json!({"isError": true, "content": []}), 0);
        assert!(cache.get("s", "get_x", &json!({}), 0).is_none());
    }

    #[test]
    fn write_then_read_sees_the_write() {
        let cache = small(16);
        cache.put("memory", "search_memory", &json!({"q": "x"}), json!("old"), 0);
        cache.put("other", "get_y", &json!({}), json!("kept"), 0);
        cache.invalidate_server("memory");
        assert!(cache.get("memory", "search_memory", &json!({"q": "x"}), 0).is_none());
        assert!(cache.get("other", "get_y", &json!({}), 0).is_some());
    }

    #[test]
    fn restarted_server_is_not_served_from_cache() {
        let cache = small(16);
        cache.put("s", "get_x", &json!({}), json!(1), 3);
        assert!(cache.get("s", "get_x", &json!({}), 4).is_none());
    }

    #[test]
    fn size_limits_are_enforced() {
        let cache = ResponseCache::with_limits(60, 100, 64, 100);
        cache.put("s", "get_big", &json!({}), json!("x".repeat(200)), 0);
        assert!(cache.get("s", "get_big", &json!({}), 0).is_none());
        cache.put("s", "get_a", &json!({}), json!("a".repeat(40)), 0);
        cache.put("s", "get_b", &json!({}), json!("b".repeat(40)), 0);
        cache.put("s", "get_c", &json!({}), json!("c".repeat(40)), 0);
        let kept = ["get_a", "get_b", "get_c"]
            .iter()
            .filter(|t| cache.get("s", t, &json!({}), 0).is_some())
            .count();
        assert_eq!(kept, 2, "100 octets au plus : deux entrees de 42 octets");
    }
}
