//! In-memory LRU cache for idempotent tool responses.
//!
//! Why: many child-server calls during a session are pure reads (read_file, list_dir, get_*).
//! Re-spawning the child + re-running the call costs latency + tokens for identical data.
//!
//! Strategy:
//! - Cache only read tools: name matching an idempotency heuristic (read/list/get/search/find/...),
//!   or MCP annotation `readOnlyHint: true`, or listed in `settings.responseCache.tools`.
//!   `readOnlyHint: false` always wins over the name.
//! - Key = (server, tool, hash(args)), and the full key (server, tool, canonical args) is stored
//!   and compared before serving: a hash collision is a miss, never a wrong answer.
//! - TTL from `settings.responseCache.ttlSecs` (default 60s, 0 disables the cache).
//! - Never cache an error result (`isError`): a transient failure must not be replayed.
//! - Any non-cacheable call on a server drops that server's entries: a write followed by a read
//!   (add_memory then search_memory) must see the write.
//! - Entries carry the child manager's epoch: a server restarted, lost or reconfigured since the
//!   entry was stored is never served from cache.
//! - Bounded by entry count, by size per entry and by total size (LRU eviction).
//! - Caller can force a fresh call with `bypass_cache: true` in mcphub_call args.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const DEFAULT_TTL_SECS: u64 = 60;
pub const MAX_TTL_SECS: u64 = 3600;
const DEFAULT_MAX_ENTRIES: usize = 256;
const DEFAULT_MAX_ENTRY_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Tools matching ANY of these prefixes are considered safe to cache.
/// Conservative on purpose: better to skip a cacheable read than to serve stale state.
const IDEMPOTENT_PREFIXES: &[&str] = &[
    "read_",
    "read-",
    "list_",
    "list-",
    "get_",
    "get-",
    "fetch_",
    "fetch-",
    "search_",
    "search-",
    "find_",
    "find-",
    "info_",
    "info-",
    "show_",
    "show-",
    "view_",
    "view-",
    "describe_",
    "describe-",
    "inspect_",
    "inspect-",
    "head_",
    "head-",
    "tail_",
    "tail-",
    "count_",
    "count-",
    "exists",
    "has_",
    "has-",
    "lookup_",
    "lookup-",
    "preview_",
    "preview-",
    "render_",
    "render-",
];

/// Tools matching ANY of these patterns are NEVER cached, even if they happen to start with a
/// read prefix (e.g. `read_then_modify`). Mutating ops must always hit the live server.
const MUTATING_PATTERNS: &[&str] = &[
    "write", "delete", "remove", "create", "update", "set_", "set-", "apply", "execute", "exec_",
    "commit", "push", "add_", "add-", "modify", "patch", "edit", "drop", "insert", "upload",
    "send_", "send-", "post_", "post-", "put_", "put-", "publish", "deploy", "kill", "restart",
    "stop", "start_", "start-", "spawn", "fork", "trigger", "invoke_", "invoke-",
];

/// `settings.responseCache` : durée de vie et outils explicitement cacheables.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheSettings {
    /// Durée de vie d'une entrée, en secondes. 0 désactive le cache.
    pub ttl_secs: u64,
    /// Outils toujours cacheables, sous la forme "serveur__outil".
    pub tools: HashSet<String>,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_TTL_SECS,
            tools: HashSet::new(),
        }
    }
}

impl CacheSettings {
    /// Une valeur invalide est une erreur : jamais de retour silencieux au défaut.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let Some(object) = value.as_object() else {
            return Err("settings.responseCache doit etre un objet".into());
        };
        let mut settings = Self::default();
        if let Some(ttl) = object.get("ttlSecs") {
            settings.ttl_secs = ttl
                .as_u64()
                .filter(|secs| *secs <= MAX_TTL_SECS)
                .ok_or_else(|| {
                    format!(
                        "settings.responseCache.ttlSecs doit etre un entier entre 0 et {} (0 desactive le cache)",
                        MAX_TTL_SECS
                    )
                })?;
        }
        if let Some(tools) = object.get("tools") {
            let Some(list) = tools.as_array() else {
                return Err(
                    "settings.responseCache.tools doit etre une liste de \"serveur__outil\"".into(),
                );
            };
            for item in list {
                let valid = item.as_str().filter(|name| {
                    name.split_once("__")
                        .is_some_and(|(server, tool)| !server.is_empty() && !tool.is_empty())
                });
                let Some(name) = valid else {
                    return Err(format!(
                        "settings.responseCache.tools : {} n'est pas de la forme \"serveur__outil\"",
                        item
                    ));
                };
                settings.tools.insert(name.to_string());
            }
        }
        Ok(settings)
    }

    /// Cet appel peut-il être servi depuis le cache ?
    /// 1. cache désactivé (ttl 0) : non ;
    /// 2. outil listé dans `tools` : oui (choix explicite de l'utilisateur) ;
    /// 3. `readOnlyHint: false` déclaré par le serveur : non, quel que soit le nom ;
    /// 4. motif mutant dans le nom : non, même avec `readOnlyHint: true` ;
    /// 5. `readOnlyHint: true` ou préfixe de lecture : oui.
    pub fn is_cacheable(&self, server: &str, tool: &str, read_only_hint: Option<bool>) -> bool {
        if self.ttl_secs == 0 {
            return false;
        }
        if self.tools.contains(&format!("{}__{}", server, tool)) {
            return true;
        }
        if read_only_hint == Some(false) || ResponseCache::has_mutating_name(tool) {
            return false;
        }
        read_only_hint == Some(true) || ResponseCache::is_cacheable(tool)
    }
}

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub value: Value,
    pub age_ms: u64,
}

struct Entry {
    server: String,
    tool: String,
    /// Arguments canoniques : comparés avant de servir, le hash seul ne suffit pas.
    args_canonical: String,
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
    /// Nombre d'invalidations par serveur : une lecture lancée avant une écriture et
    /// terminée après elle ne doit pas entrer dans le cache avec l'état d'avant.
    generations: HashMap<String, u64>,
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

type KeyHasher = fn(&str, &str, &str) -> u64;

fn default_key_hash(server: &str, tool: &str, args_canonical: &str) -> u64 {
    // DefaultHasher (SipHash) is plenty for cache keying — not cryptographic.
    let mut hasher = DefaultHasher::new();
    server.hash(&mut hasher);
    tool.hash(&mut hasher);
    args_canonical.hash(&mut hasher);
    hasher.finish()
}

/// serde_json trie les clés d'objet (pas de `preserve_order`) : deux objets égaux donnent le
/// même texte, quel que soit l'ordre d'écriture des clés par l'appelant.
fn canonical_args(args: &Value) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| "null".to_string())
}

pub struct ResponseCache {
    store: Mutex<Store>,
    ttl_secs: AtomicU64,
    key_hasher: KeyHasher,
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
            ttl_secs: AtomicU64::new(ttl_secs),
            key_hasher: default_key_hash,
            max_entries,
            max_entry_bytes,
            max_total_bytes,
        }
    }

    /// Durée de vie appliquée à partir de maintenant (rechargée avec la config). Une entrée
    /// plus vieille que la nouvelle durée n'est plus servie ; 0 vide et désactive le cache.
    pub fn set_ttl_secs(&self, ttl_secs: u64) {
        if self.ttl_secs.swap(ttl_secs, Ordering::Relaxed) != ttl_secs && ttl_secs == 0 {
            if let Ok(mut store) = self.store.lock() {
                store.retain(|_| false);
            }
        }
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.load(Ordering::Relaxed))
    }

    /// Mutating ops must always hit the live server, whatever their prefix or annotations.
    fn has_mutating_name(tool: &str) -> bool {
        let lower = tool.to_lowercase();
        MUTATING_PATTERNS.iter().any(|p| lower.contains(p))
    }

    /// Returns true if this tool name alone is safe to cache.
    /// Strictly: idempotent prefix AND no mutating pattern anywhere in the name.
    pub fn is_cacheable(tool: &str) -> bool {
        if Self::has_mutating_name(tool) {
            return false;
        }
        let lower = tool.to_lowercase();
        IDEMPOTENT_PREFIXES.iter().any(|p| lower.starts_with(p))
    }

    fn make_key(&self, server: &str, tool: &str, args_canonical: &str) -> String {
        format!(
            "{}__{}__{:016x}",
            server,
            tool,
            (self.key_hasher)(server, tool, args_canonical)
        )
    }

    pub fn get(
        &self,
        server: &str,
        tool: &str,
        args: &Value,
        epoch: u64,
    ) -> Option<CachedResponse> {
        let ttl = self.ttl();
        if ttl.is_zero() {
            return None;
        }
        let args_canonical = canonical_args(args);
        let key = self.make_key(server, tool, &args_canonical);
        let mut store = self.store.lock().ok()?;
        let entry = store.entries.get_mut(&key)?;
        // Même hash, autre appel : ce n'est pas la réponse demandée.
        if entry.server != server || entry.tool != tool || entry.args_canonical != args_canonical {
            return None;
        }
        let age = entry.stored_at.elapsed();
        if age > ttl || entry.epoch != epoch {
            store.remove(&key);
            return None;
        }
        entry.last_accessed = Instant::now();
        Some(CachedResponse {
            value: entry.value.clone(),
            age_ms: age.as_millis() as u64,
        })
    }

    /// À relever avant l'appel au serveur, puis à rendre à `put_if_current`.
    pub fn generation(&self, server: &str) -> u64 {
        self.store
            .lock()
            .map(|store| store.generations.get(server).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn put(&self, server: &str, tool: &str, args: &Value, value: Value, epoch: u64) {
        let generation = self.generation(server);
        self.put_if_current(server, tool, args, value, epoch, generation);
    }

    /// N'enregistre rien si le serveur a été invalidé depuis `generation` : la réponse a pu
    /// être lue avant une écriture qui s'est terminée entre-temps.
    pub fn put_if_current(
        &self,
        server: &str,
        tool: &str,
        args: &Value,
        value: Value,
        epoch: u64,
        generation: u64,
    ) {
        let ttl = self.ttl();
        if ttl.is_zero() {
            return;
        }
        if value.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return;
        }
        let bytes = serde_json::to_string(&value)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if bytes > self.max_entry_bytes {
            return;
        }
        let args_canonical = canonical_args(args);
        let key = self.make_key(server, tool, &args_canonical);
        let now = Instant::now();
        let Ok(mut store) = self.store.lock() else {
            return;
        };
        if store.generations.get(server).copied().unwrap_or(0) != generation {
            return;
        }
        store.remove(&key);
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
                tool: tool.to_string(),
                args_canonical,
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
            *store.generations.entry(server.to_string()).or_insert(0) += 1;
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

    #[test]
    fn lecture_lancee_avant_une_ecriture_jamais_mise_en_cache() {
        let cache = ResponseCache::new();
        let args = json!({});
        // La lecture lente part, puis une écriture invalide le serveur avant son retour.
        let generation = cache.generation("db");
        cache.invalidate_server("db");
        cache.put_if_current(
            "db",
            "list_items",
            &args,
            json!({"items": []}),
            0,
            generation,
        );
        assert!(cache.get("db", "list_items", &args, 0).is_none());

        // Une lecture lancée après l'écriture entre normalement, sans gêner un autre serveur.
        let generation = cache.generation("db");
        cache.put_if_current(
            "db",
            "list_items",
            &args,
            json!({"items": ["x"]}),
            0,
            generation,
        );
        assert_eq!(
            cache.get("db", "list_items", &args, 0).map(|hit| hit.value),
            Some(json!({"items": ["x"]}))
        );
        cache.invalidate_server("other");
        assert!(cache.get("db", "list_items", &args, 0).is_some());
    }

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
    fn ttl_zero_desactive_le_cache() {
        let cache = ResponseCache::with_limits(0, 16, 1024, 1024 * 1024);
        cache.put("s", "get_x", &json!({}), json!(1), 0);
        assert!(cache.get("s", "get_x", &json!({}), 0).is_none());
        let off = CacheSettings {
            ttl_secs: 0,
            tools: ["s__get_x".to_string()].into(),
        };
        assert!(!off.is_cacheable("s", "get_x", Some(true)));

        // Passage a 0 en cours de route (config rechargee) : les entrees existantes tombent.
        let cache = small(16);
        cache.put("s", "get_x", &json!({}), json!(1), 0);
        cache.set_ttl_secs(0);
        assert!(cache.get("s", "get_x", &json!({}), 0).is_none());
        cache.set_ttl_secs(60);
        assert!(
            cache.get("s", "get_x", &json!({}), 0).is_none(),
            "rien ne revient apres reactivation"
        );
        cache.put("s", "get_x", &json!({}), json!(2), 0);
        assert_eq!(
            cache.get("s", "get_x", &json!({}), 0).unwrap().value,
            json!(2)
        );
    }

    #[test]
    fn read_only_hint_decide_avant_le_nom() {
        let settings = CacheSettings::default();
        // true : cacheable meme sans prefixe de lecture.
        assert!(settings.is_cacheable("context7", "resolve-library-id", Some(true)));
        assert!(!settings.is_cacheable("context7", "resolve-library-id", None));
        // true mais nom mutant : jamais.
        assert!(!settings.is_cacheable("db", "execute_sql", Some(true)));
        assert!(!settings.is_cacheable("db", "read_then_modify", Some(true)));
        // false explicite : jamais, meme avec un prefixe de lecture.
        assert!(!settings.is_cacheable("s", "get_user", Some(false)));
        // Sans annotation : la regle par nom reste inchangee.
        assert!(settings.is_cacheable("s", "get_user", None));
        assert!(!settings.is_cacheable("s", "write_file", None));
    }

    #[test]
    fn liste_explicite_toujours_cacheable() {
        let settings = CacheSettings::from_json(&json!({
            "ttlSecs": 120,
            "tools": ["context7__resolve-library-id"]
        }))
        .unwrap();
        assert_eq!(settings.ttl_secs, 120);
        assert!(settings.is_cacheable("context7", "resolve-library-id", None));
        assert!(
            !settings.is_cacheable("autre", "resolve-library-id", None),
            "le serveur compte"
        );
        assert!(!settings.is_cacheable("context7", "query-docs", None));
    }

    #[test]
    fn reglages_invalides_refuses() {
        assert_eq!(
            CacheSettings::from_json(&json!({})).unwrap(),
            CacheSettings::default()
        );
        assert_eq!(
            CacheSettings::from_json(&json!({"ttlSecs": 0}))
                .unwrap()
                .ttl_secs,
            0
        );
        assert_eq!(
            CacheSettings::from_json(&json!({"ttlSecs": 3600}))
                .unwrap()
                .ttl_secs,
            3600
        );
        for invalid in [
            json!(60),
            json!({"ttlSecs": 3601}),
            json!({"ttlSecs": -1}),
            json!({"ttlSecs": "60"}),
            json!({"ttlSecs": 1.5}),
            json!({"tools": "context7__resolve-library-id"}),
            json!({"tools": ["resolve-library-id"]}),
            json!({"tools": ["__outil"]}),
            json!({"tools": [42]}),
        ] {
            assert!(
                CacheSettings::from_json(&invalid).is_err(),
                "{} doit etre refuse",
                invalid
            );
        }
    }

    #[test]
    fn collision_de_hash_ne_sert_pas_la_mauvaise_reponse() {
        let mut cache = small(16);
        // Tous les appels tombent sur le meme hash.
        cache.key_hasher = |_, _, _| 42;
        cache.put(
            "fs",
            "read_file",
            &json!({"path": "a"}),
            json!("contenu de a"),
            0,
        );
        assert!(
            cache
                .get("fs", "read_file", &json!({"path": "b"}), 0)
                .is_none(),
            "meme hash, autres arguments : pas de reponse"
        );
        assert_eq!(
            cache
                .get("fs", "read_file", &json!({"path": "a"}), 0)
                .unwrap()
                .value,
            json!("contenu de a")
        );
        // L'ordre des cles ne change pas l'appel.
        cache.put("fs", "read_x", &json!({"a": 1, "b": 2}), json!("ab"), 0);
        assert!(cache
            .get("fs", "read_x", &json!({"b": 2, "a": 1}), 0)
            .is_some());
    }

    #[test]
    fn put_get_roundtrip() {
        let cache = small(16);
        let args = json!({"path": "/tmp/x"});
        cache.put(
            "filesystem",
            "read_file",
            &args,
            json!({"content": "hello"}),
            0,
        );
        let hit = cache.get("filesystem", "read_file", &args, 0).unwrap();
        assert_eq!(hit.value, json!({"content": "hello"}));
    }

    #[test]
    fn different_args_produce_different_keys() {
        let cache = small(16);
        cache.put("fs", "read_file", &json!({"path": "a"}), json!(1), 0);
        cache.put("fs", "read_file", &json!({"path": "b"}), json!(2), 0);
        assert_eq!(
            cache
                .get("fs", "read_file", &json!({"path": "a"}), 0)
                .unwrap()
                .value,
            json!(1)
        );
        assert_eq!(
            cache
                .get("fs", "read_file", &json!({"path": "b"}), 0)
                .unwrap()
                .value,
            json!(2)
        );
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
        cache.put(
            "s",
            "get_x",
            &json!({}),
            json!({"isError": true, "content": []}),
            0,
        );
        assert!(cache.get("s", "get_x", &json!({}), 0).is_none());
    }

    #[test]
    fn write_then_read_sees_the_write() {
        let cache = small(16);
        cache.put(
            "memory",
            "search_memory",
            &json!({"q": "x"}),
            json!("old"),
            0,
        );
        cache.put("other", "get_y", &json!({}), json!("kept"), 0);
        cache.invalidate_server("memory");
        assert!(cache
            .get("memory", "search_memory", &json!({"q": "x"}), 0)
            .is_none());
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
