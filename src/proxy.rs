/// Core proxy server: reads JSON-RPC from stdin, routes to child servers.
/// Two modes: discover (2 meta-tools) or passthrough (all tools exposed).
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::audit::{AuditConfig, AuditEntry, AuditLogger};
use crate::child::ChildManager;
use crate::config::{Mode, ProxyConfig};
use crate::crawl;
use crate::health::HealthMonitor;
use crate::permissions::{Action, ConfirmStore};
use crate::protocol::*;
use crate::response_cache::ResponseCache;
use crate::search::{IndexedTool, SearchEngine};
use crate::untrusted;

tokio::task_local! {
    /// Profil du client à l'origine de la requête en cours (ex. /mcp/cursor).
    static REQUEST_PROFILE: Option<String>;
    /// Appel lancé depuis le code mode : nombre de fenêtres de confirmation déjà ouvertes.
    static CODE_RUN: std::sync::Arc<std::sync::atomic::AtomicUsize>;
}

/// Fenêtres de confirmation au plus par exécution de `mcphub_run`.
const MAX_DIALOGS_PER_RUN: usize = 3;

/// Une fenêtre de confirmation est ouverte : les serveurs capables de piloter l'ordinateur
/// (et donc de cliquer « Autoriser » à la place de l'utilisateur) sont mis en pause.
static DIALOG_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn controls_computer(server: &str) -> bool {
    let s = server.to_ascii_lowercase();
    ["desktop-commander", "computer", "applescript", "osascript", "macos-automator", "peekaboo"]
        .iter()
        .any(|k| s.contains(k))
}

/// Refus récents (empreinte de l'appel) : on ne redemande pas la même chose pendant 5 minutes.
fn recent_denials() -> std::sync::MutexGuard<'static, HashMap<String, Instant>> {
    static D: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    D.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Profil actif : celui de la requête HTTP, sinon la variable MCPHUB_PROFILE (mode stdio).
pub fn current_profile() -> Option<String> {
    REQUEST_PROFILE
        .try_with(|p| p.clone())
        .ok()
        .flatten()
        .or_else(|| {
            std::env::var("MCPHUB_PROFILE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerMetrics {
    pub call_count: u64,
    pub error_count: u64,
    pub total_latency_ms: u64,
    pub last_call_time: Option<SystemTime>,
    pub last_error: Option<String>,
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self {
            call_count: 0,
            error_count: 0,
            total_latency_ms: 0,
            last_call_time: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobalMetrics {
    pub start_time: SystemTime,
    pub total_requests: u64,
    pub active_sse_sessions: usize,
    pub discover_routing: DiscoverRoutingMetrics,
    pub execute_routing: ExecuteRoutingMetrics,
    pub servers: HashMap<String, ServerMetrics>,
    /// Per-tool call counts (key = "server__tool"). Used to boost BM25 ranking
    /// so frequently-used tools surface first. Persisted to ~/.McpHub/usage-stats.json.
    pub tool_calls: HashMap<String, u64>,
}

impl GlobalMetrics {
    pub fn new() -> Self {
        Self {
            start_time: SystemTime::now(),
            total_requests: 0,
            active_sse_sessions: 0,
            discover_routing: DiscoverRoutingMetrics::default(),
            execute_routing: ExecuteRoutingMetrics::default(),
            servers: HashMap::new(),
            tool_calls: load_usage_stats(),
        }
    }
}

fn usage_stats_path() -> std::path::PathBuf {
    crate::instance::hub_dir().join("usage-stats.json")
}

fn load_usage_stats() -> HashMap<String, u64> {
    let path = usage_stats_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_usage_stats(stats: &HashMap<String, u64>) {
    let path = usage_stats_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(stats) {
        let _ = std::fs::write(&path, json);
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverRouteStrategy {
    IndexedBm25,
    LiveServerLookup,
    ServerListFallback,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverFilterSource {
    ExplicitServer,
    QueryServerName,
    QueryToolName,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteRouteStrategy {
    ExactCatalogMatch,
    CaseInsensitiveCatalogMatch,
    UncataloguedBypass,
    UnknownToolError,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct DiscoverRoutingMetrics {
    pub total: u64,
    pub explicit_server: u64,
    pub inferred_server_name: u64,
    pub inferred_tool_name: u64,
    pub indexed_bm25: u64,
    pub live_server_lookup: u64,
    pub server_list_fallback: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ExecuteRoutingMetrics {
    pub total: u64,
    pub exact_catalog_match: u64,
    pub case_insensitive_catalog_match: u64,
    pub uncatalogued_bypass: u64,
    pub unknown_tool_error: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoverRoutingInfo {
    pub strategy: DiscoverRouteStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_source: Option<DiscoverFilterSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_filter: Option<String>,
    pub reason: String,
}

pub struct ProxyServer {
    config: Arc<Mutex<ProxyConfig>>,
    child_manager: Arc<ChildManager>,
    search_engine: Arc<Mutex<SearchEngine>>,
    pub metrics: Arc<Mutex<GlobalMetrics>>,
    catalog_refresh_in_progress: Arc<AtomicBool>,
    response_cache: Arc<ResponseCache>,
    confirmations: Arc<ConfirmStore>,
    audit: Arc<AuditLogger>,
}

/// Résultat du contrôle de permission avant un appel d'outil.
enum PolicyGate {
    /// L'appel peut partir. `decision` = "allow" ou "confirmed", `policy` = source de la règle.
    Proceed {
        decision: &'static str,
        policy: String,
    },
    /// Réponse déjà prête (blocage ou demande de confirmation), à renvoyer telle quelle.
    Respond(JsonRpcResponse),
}

impl ProxyServer {
    pub fn new(config: ProxyConfig) -> Self {
        crate::toolguard::configure(config.guard.clone());
        let child_manager = Arc::new(ChildManager::new(
            config.servers.clone(),
            config.idle_timeout_ms,
            config.request_timeout_secs,
        ));

        Self {
            config: Arc::new(Mutex::new(config)),
            child_manager,
            search_engine: Arc::new(Mutex::new(SearchEngine::new())),
            metrics: Arc::new(Mutex::new(GlobalMetrics::new())),
            catalog_refresh_in_progress: Arc::new(AtomicBool::new(false)),
            response_cache: Arc::new(ResponseCache::new()),
            confirmations: Arc::new(ConfirmStore::new()),
            audit: Arc::new(AuditLogger::new(crate::audit::default_path())),
        }
    }

    async fn audit_config(&self) -> AuditConfig {
        self.config.lock().await.audit.clone()
    }

    async fn record_audit(&self, entry: AuditEntry) {
        let cfg = self.audit_config().await;
        self.audit.record(&cfg, entry);
    }

    /// Applique les permissions (allow / confirm / deny) avant tout appel d'outil,
    /// y compris quand la réponse viendrait du cache.
    async fn policy_gate(
        &self,
        id: &Option<serde_json::Value>,
        mode: &'static str,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
        confirmation_token: Option<&str>,
    ) -> PolicyGate {
        let permissions = self.config.lock().await.permissions.clone();
        // Toujours partir de la définition EN DIRECT : le serveur est démarré (ou réveillé)
        // avant le contrôle, donc épinglé et analysé avant que l'appel parte. Démarrer un
        // serveur déjà actif ne coûte qu'un verrou.
        let live = self.child_manager.start_server(server).await;
        let (tool_def, live_block) = match &live {
            Ok(tools) => match tools.iter().find(|t| t.name == tool) {
                Some(def) => (Some(def.clone()), None),
                None => (
                    None,
                    Some(format!(
                        "outil '{}' non annonce par le serveur (verifier le nom avec mcphub_search)",
                        tool
                    )),
                ),
            },
            Err(e) => (None, Some(format!("serveur indisponible : {}", e))),
        };
        let reason = live_block.or_else(|| {
            crate::toolguard::block_reason(server, tool, tool_def.as_ref(), current_profile().as_deref())
        });
        if let Some(reason) = reason {
            self.record_audit(AuditEntry {
                server: server.to_string(),
                tool: tool.to_string(),
                mode,
                decision: "deny",
                status: "blocked",
                policy: "toolguard".into(),
                arguments: Some(arguments.clone()),
                ..Default::default()
            })
            .await;
            return PolicyGate::Respond(JsonRpcResponse::error_with_data(
                id.clone(),
                -32001,
                format!("Bloque par McpHub : {}/{} : {}", server, tool, reason),
                serde_json::json!({
                    "server": server,
                    "tool": tool,
                    "policy": "toolguard",
                    "hint": "Ne pas reessayer ni chercher a contourner : le signaler a l'utilisateur."
                }),
            ));
        }
        let declared_destructive = tool_def
            .as_ref()
            .map(|t| t.declared_destructive())
            .unwrap_or(false);
        let decision = permissions.decide_with(server, tool, declared_destructive);
        self.permission_gate(id, mode, server, tool, arguments, confirmation_token, &permissions, decision)
            .await
    }

    /// Outils internes qui installent et lancent un programme (`server: "mcphub"`) : une règle
    /// explicite l'emporte, sinon confirmation humaine, quel que soit `default`.
    async fn hub_tool_gate(
        &self,
        id: &Option<serde_json::Value>,
        tool: &str,
        arguments: &serde_json::Value,
        confirmation_token: Option<&str>,
    ) -> PolicyGate {
        let permissions = self.config.lock().await.permissions.clone();
        let decision = permissions
            .rule_for("mcphub", tool)
            .unwrap_or_else(|| crate::permissions::Decision {
                action: Action::Confirm,
                source: "mcphub:install".into(),
                reason: Some("installe et lance un nouveau serveur MCP sur cette machine".into()),
            });
        self.permission_gate(id, "library", "mcphub", tool, arguments, confirmation_token, &permissions, decision)
            .await
    }

    async fn permission_gate(
        &self,
        id: &Option<serde_json::Value>,
        mode: &'static str,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
        confirmation_token: Option<&str>,
        permissions: &crate::permissions::PermissionsConfig,
        decision: crate::permissions::Decision,
    ) -> PolicyGate {
        if DIALOG_OPEN.load(std::sync::atomic::Ordering::SeqCst) && controls_computer(server) {
            return PolicyGate::Respond(JsonRpcResponse::error(
                id.clone(),
                -32001,
                format!(
                    "{} : en pause pendant qu'une confirmation McpHub attend l'utilisateur. Reessayer apres sa reponse.",
                    server
                ),
            ));
        }
        let base_entry = || AuditEntry {
            server: server.to_string(),
            tool: tool.to_string(),
            mode,
            policy: decision.source.clone(),
            arguments: Some(arguments.clone()),
            ..Default::default()
        };
        match decision.action {
            Action::Allow => PolicyGate::Proceed {
                decision: "allow",
                policy: decision.source.clone(),
            },
            Action::Deny => {
                self.record_audit(AuditEntry {
                    decision: "deny",
                    status: "blocked",
                    ..base_entry()
                })
                .await;
                let reason = decision
                    .reason
                    .clone()
                    .map(|r| format!(" Raison : {}.", r))
                    .unwrap_or_default();
                PolicyGate::Respond(JsonRpcResponse::error_with_data(
                    id.clone(),
                    -32001,
                    format!(
                        "Bloque par la politique McpHub ({}) : {}/{} est interdit.{}",
                        decision.source, server, tool, reason
                    ),
                    serde_json::json!({
                        "server": server,
                        "tool": tool,
                        "policy": decision.source,
                        "hint": "Cet appel est interdit par ~/.McpHub/config.json (settings.permissions). Ne pas reessayer : le signaler a l'utilisateur."
                    }),
                ))
            }
            Action::Confirm => {
                let fp = crate::permissions::fingerprint(server, tool, arguments);
                // Confirmation humaine d'abord : fenêtre macOS que l'agent ne peut pas valider.
                if permissions.confirm_via_dialog && confirmation_token.is_none() {
                    let policy_src = decision.source.clone();
                    let deny = |msg: String, decision_label: &'static str| {
                        let entry = AuditEntry {
                            decision: decision_label,
                            status: "blocked",
                            ..base_entry()
                        };
                        let policy_src = policy_src.clone();
                        let rid = id.clone();
                        async move {
                        self.record_audit(entry).await;
                        PolicyGate::Respond(JsonRpcResponse::error_with_data(
                            rid,
                            -32001,
                            msg,
                            serde_json::json!({
                                "server": server,
                                "tool": tool,
                                "policy": policy_src,
                                "hint": "Ne pas reessayer automatiquement : demander a l'utilisateur ce qu'il souhaite."
                            }),
                        ))
                        }
                    };
                    // Même demande refusée il y a moins de 5 minutes : pas de harcèlement.
                    let recently_denied = {
                        let mut d = recent_denials();
                        d.retain(|_, t| t.elapsed() < std::time::Duration::from_secs(300));
                        d.contains_key(&fp)
                    };
                    if recently_denied {
                        return deny(
                            format!("{}/{} : refuse par l'utilisateur il y a moins de 5 minutes.", server, tool),
                            "human_denied",
                        )
                        .await;
                    }
                    let in_code = CODE_RUN.try_with(|c| c.clone()).ok();
                    if let Some(counter) = &in_code {
                        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_DIALOGS_PER_RUN {
                            return deny(
                                format!("Code mode : plus de {} confirmations dans une meme execution, refuse.", MAX_DIALOGS_PER_RUN),
                                "deny",
                            )
                            .await;
                        }
                    }
                    let reason = decision
                        .reason
                        .clone()
                        .unwrap_or_else(|| "action sensible".into());
                    let mut origin = match current_profile() {
                        Some(p) => format!("mode {}, profil {}", mode, p),
                        None => format!("mode {}", mode),
                    };
                    if in_code.is_some() {
                        origin.push_str(", code mode (mcphub_run)");
                    }
                    let preview = crate::audit::safe_arguments(arguments);
                    let message =
                        crate::confirm_dialog::build_message(server, tool, &reason, &preview, &origin);
                    DIALOG_OPEN.store(true, std::sync::atomic::Ordering::SeqCst);
                    let answer = crate::confirm_dialog::ask(message, permissions.dialog_timeout_secs).await;
                    DIALOG_OPEN.store(false, std::sync::atomic::Ordering::SeqCst);
                    match answer {
                        crate::confirm_dialog::DialogAnswer::Allow => {
                            return PolicyGate::Proceed {
                                decision: "confirmed_human",
                                policy: decision.source.clone(),
                            };
                        }
                        crate::confirm_dialog::DialogAnswer::Deny => {
                            recent_denials().insert(fp.clone(), Instant::now());
                            return deny(
                                format!("L'utilisateur a refuse {}/{} dans la fenetre de confirmation McpHub.", server, tool),
                                "human_denied",
                            )
                            .await;
                        }
                        crate::confirm_dialog::DialogAnswer::Timeout => {
                            return deny(
                                format!("Pas de reponse de l'utilisateur dans la fenetre de confirmation McpHub : {}/{} non execute.", server, tool),
                                "human_timeout",
                            )
                            .await;
                        }
                        crate::confirm_dialog::DialogAnswer::Unavailable => {
                            if !permissions.dialog_fallback_token {
                                return deny(
                                    format!("{}/{} demande une confirmation, mais aucune fenetre ne peut s'afficher (session sans ecran). Refuse.", server, tool),
                                    "deny",
                                )
                                .await;
                            }
                            // confirmVia "auto" : repli sur le jeton ci-dessous.
                        }
                    }
                } else if permissions.confirm_via_dialog
                    && !permissions.dialog_fallback_token
                    && confirmation_token.is_some()
                {
                    // Mode fenêtre seule : aucun jeton n'est jamais émis, donc aucun n'est valable.
                    self.record_audit(AuditEntry {
                        decision: "invalid_token",
                        status: "blocked",
                        ..base_entry()
                    })
                    .await;
                    return PolicyGate::Respond(JsonRpcResponse::error(
                        id.clone(),
                        -32001,
                        "Confirmation par jeton desactivee : rappeler sans jeton, une fenetre s'ouvrira pour l'utilisateur.".into(),
                    ));
                }
                match confirmation_token {
                    Some(token) if self.confirmations.consume(token, &fp) => PolicyGate::Proceed {
                        decision: "confirmed",
                        policy: decision.source.clone(),
                    },
                    Some(_) => {
                        self.record_audit(AuditEntry {
                            decision: "invalid_token",
                            status: "blocked",
                            ..base_entry()
                        })
                        .await;
                        PolicyGate::Respond(JsonRpcResponse::error(
                            id.clone(),
                            -32001,
                            "Jeton de confirmation invalide, expire, deja utilise, ou arguments differents de ceux confirmes. Refaire l'appel sans jeton pour en obtenir un nouveau, et redemander l'accord de l'utilisateur.".into(),
                        ))
                    }
                    None => {
                        let token = self.confirmations.issue(&fp, permissions.confirm_ttl_secs);
                        self.record_audit(AuditEntry {
                            decision: "confirm_requested",
                            status: "pending",
                            ..base_entry()
                        })
                        .await;
                        let preview = crate::audit::safe_arguments(arguments);
                        let reason = decision
                            .reason
                            .clone()
                            .unwrap_or_else(|| "action sensible".into());
                        let text = format!(
                            "CONFIRMATION REQUISE par McpHub ({}, {}).\n\
                             Appel : serveur `{}`, outil `{}`\n\
                             Arguments : {}\n\n\
                             Montre cet appel a l'utilisateur et demande-lui un accord explicite. \
                             Seulement s'il accepte, rappelle le meme outil avec exactement les memes \
                             serveur, outil et arguments, plus {}: \"{}\". \
                             Le jeton est valable {} secondes et une seule fois. Ne confirme jamais a sa place.",
                            decision.source,
                            reason,
                            server,
                            tool,
                            preview,
                            if mode == "passthrough" {
                                "l'argument `_mcphub_confirmation_token` (dans les arguments de l'outil)"
                            } else {
                                "le parametre `confirmation_token` de mcphub_call"
                            },
                            token,
                            permissions.confirm_ttl_secs
                        );
                        // `_meta` est conservé par tous les clients MCP ; `_mcphub` reste pour
                        // les intégrations existantes (code mode).
                        let pending = serde_json::json!({
                            "status": "confirmation_required",
                            "confirmation_token": token,
                            "expires_in_secs": permissions.confirm_ttl_secs,
                            "server": server,
                            "tool": tool,
                            "policy": decision.source,
                        });
                        PolicyGate::Respond(JsonRpcResponse::success(
                            id.clone(),
                            serde_json::json!({
                                "content": [{ "type": "text", "text": text }],
                                "isError": false,
                                "_meta": { "mcphub": pending.clone() },
                                "_mcphub": pending,
                            }),
                        ))
                    }
                }
            }
        }
    }

    /// Initialize proxy: load cache, start background tasks.
    /// Call this before stdio_loop() or serving SSE.
    pub async fn init(&self) {
        let configured_server_names = self.configured_server_names().await;
        let configured_server_hashes = {
            let config = self.config.lock().await;
            crate::cache::server_config_hashes(&config.servers)
        };
        let mut catalog_refresh_reason = None;
        let mut refresh_server_names: Vec<String> = Vec::new();
        let removed_server_names: Vec<String> = Vec::new();

        // 1. Load cache synchronously FIRST (instant, <1ms)
        if let Some(cached) = crate::cache::load_cache() {
            let all_tools = indexed_tools_from_servers(&cached.servers);
            if !all_tools.is_empty() {
                let mut eng = self.search_engine.lock().await;
                eng.build_index(all_tools);
                eprintln!(
                    "[McpHub][INFO] Ready: {} tools from cache",
                    eng.tool_count()
                );
            } else {
                catalog_refresh_reason = Some("cache is empty".to_string());
                refresh_server_names = configured_server_names.clone();
            }

            if cached.version != env!("CARGO_PKG_VERSION") {
                catalog_refresh_reason = Some(format!(
                    "cache version {} differs from binary {}",
                    cached.version,
                    env!("CARGO_PKG_VERSION")
                ));
                refresh_server_names = configured_server_names.clone();
            }

            let cached_server_names: HashSet<String> = cached
                .servers
                .keys()
                .chain(cached.errors.keys())
                .cloned()
                .collect();
            let missing_servers: Vec<String> = configured_server_names
                .iter()
                .filter(|name| !cached_server_names.contains(*name))
                .cloned()
                .collect();
            let stale_servers: Vec<String> = configured_server_names
                .iter()
                .filter(|name| {
                    cached.server_hashes.get(*name) != configured_server_hashes.get(*name)
                        || cached.errors.contains_key(*name)
                })
                .cloned()
                .collect();
            let mut refresh_candidates = stale_servers;
            for missing_server in missing_servers {
                if !refresh_candidates.contains(&missing_server) {
                    refresh_candidates.push(missing_server);
                }
            }
            if !refresh_candidates.is_empty() && catalog_refresh_reason.is_none() {
                catalog_refresh_reason = Some(format!(
                    "cache needs refresh for {} configured server(s): {}",
                    refresh_candidates.len(),
                    refresh_candidates.join(", ")
                ));
                refresh_server_names = refresh_candidates;
            }
        } else {
            eprintln!("[McpHub][WARN] No cache found. Run 'McpHub generate' for instant startup.");
            catalog_refresh_reason = Some("cache not found".to_string());
            refresh_server_names = configured_server_names.clone();
        }

        // 2. Start idle reaper
        let manager_reap = self.child_manager.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                manager_reap.reap_idle().await;
            }
        });

        // 3. Start config & cache hot-reload watcher
        let engine_watch = self.search_engine.clone();
        let config_watch = self.config.clone();
        let child_manager_watch = self.child_manager.clone();
        let refresh_watch = self.catalog_refresh_in_progress.clone();
        tokio::spawn(async move {
            config_and_cache_watcher(
                engine_watch,
                config_watch,
                child_manager_watch,
                refresh_watch,
            )
            .await;
        });

        // 4. Start health monitor (notifications + auto-restart)
        let config = self.config.lock().await;
        if config.health_notifications {
            let monitor = HealthMonitor::new(
                self.child_manager.clone(),
                config.health_check_interval_secs,
                config.health_auto_restart,
            );
            tokio::spawn(async move {
                monitor.run().await;
            });
        }

        let preload_delay_ms = config.preload_delay_ms;
        drop(config);

        if let Some(reason) = catalog_refresh_reason {
            spawn_catalog_refresh(
                reason,
                self.child_manager.clone(),
                self.search_engine.clone(),
                refresh_server_names,
                removed_server_names,
                configured_server_hashes,
                preload_delay_ms,
                self.catalog_refresh_in_progress.clone(),
            );
        }
    }

    pub async fn shutdown(&self) {
        self.child_manager.stop_all().await;
    }

    /// Minimal init for stdio mode: load cache only, no background tasks.
    /// No HTTP server, no preload, no health monitor, no idle reaper, no catalog refresh.
    /// Goal: respond to initialize + tools/list as fast and quietly as possible.
    pub async fn init_minimal(&self) {
        if let Some(cached) = crate::cache::load_cache() {
            let all_tools = indexed_tools_from_servers(&cached.servers);
            if !all_tools.is_empty() {
                let mut eng = self.search_engine.lock().await;
                eng.build_index(all_tools);
            }
        }
    }

    async fn normalize_result_for_profile(&self, result: serde_json::Value) -> serde_json::Value {
        let media_profile = {
            let config = self.config.lock().await;
            config.media_profile.clone()
        };
        normalize_child_result(result, &media_profile)
    }

    async fn record_discover_route(
        &self,
        strategy: DiscoverRouteStrategy,
        filter_source: Option<DiscoverFilterSource>,
    ) {
        let mut metrics = self.metrics.lock().await;
        metrics.discover_routing.total += 1;

        if let Some(filter_source) = filter_source {
            match filter_source {
                DiscoverFilterSource::ExplicitServer => {
                    metrics.discover_routing.explicit_server += 1;
                }
                DiscoverFilterSource::QueryServerName => {
                    metrics.discover_routing.inferred_server_name += 1;
                }
                DiscoverFilterSource::QueryToolName => {
                    metrics.discover_routing.inferred_tool_name += 1;
                }
            }
        }

        match strategy {
            DiscoverRouteStrategy::IndexedBm25 => metrics.discover_routing.indexed_bm25 += 1,
            DiscoverRouteStrategy::LiveServerLookup => {
                metrics.discover_routing.live_server_lookup += 1
            }
            DiscoverRouteStrategy::ServerListFallback => {
                metrics.discover_routing.server_list_fallback += 1
            }
        }
    }

    async fn record_execute_route(&self, strategy: ExecuteRouteStrategy) {
        let mut metrics = self.metrics.lock().await;
        metrics.execute_routing.total += 1;

        match strategy {
            ExecuteRouteStrategy::ExactCatalogMatch => {
                metrics.execute_routing.exact_catalog_match += 1
            }
            ExecuteRouteStrategy::CaseInsensitiveCatalogMatch => {
                metrics.execute_routing.case_insensitive_catalog_match += 1
            }
            ExecuteRouteStrategy::UncataloguedBypass => {
                metrics.execute_routing.uncatalogued_bypass += 1
            }
            ExecuteRouteStrategy::UnknownToolError => {
                metrics.execute_routing.unknown_tool_error += 1
            }
        }
    }

    async fn configured_server_names(&self) -> Vec<String> {
        let config = self.config.lock().await;
        let mut server_names: Vec<String> = config.servers.keys().cloned().collect();
        server_names.sort();
        server_names
    }

    pub async fn stdio_loop(&self) {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        let debug = std::env::var("MCPHUB_STDIO_DEBUG").ok().as_deref() == Some("1");

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    if debug {
                        eprintln!("[McpHub][STDIO] parse error: {} | line: {}", e, line);
                    }
                    continue;
                }
            };

            if debug {
                eprintln!(
                    "[McpHub][STDIO] <- method={} id={:?}",
                    request.method, request.id
                );
            }

            let method_for_log = request.method.clone();
            let id_for_log = request.id.clone();
            let response = self.handle_request(request).await;

            if let Some(resp) = response {
                let mut msg = serde_json::to_string(&resp).unwrap();
                msg.push('\n');
                if debug {
                    eprintln!(
                        "[McpHub][STDIO] -> method={} id={:?} bytes={}",
                        method_for_log,
                        id_for_log,
                        msg.len()
                    );
                }
                let _ = stdout.write_all(msg.as_bytes()).await;
                let _ = stdout.flush().await;
            } else if debug {
                eprintln!(
                    "[McpHub][STDIO] -> (no response) method={} id={:?}",
                    method_for_log, id_for_log
                );
            }
        }

        // Cleanup
        self.child_manager.stop_all().await;
    }

    /// Traite une requête dans le contexte d'un profil client (HTTP /mcp/<profil>, SSE ?profile=).
    pub async fn handle_request_in_profile(
        &self,
        req: JsonRpcRequest,
        profile: Option<String>,
    ) -> Option<JsonRpcResponse> {
        REQUEST_PROFILE.scope(profile, self.handle_request(req)).await
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        match req.method.as_str() {
            "initialize" => Some(self.handle_initialize(req.id, req.params).await),
            "notifications/initialized" => None,
            "tools/list" => Some(self.handle_tools_list(req.id).await),
            "tools/call" => Some(self.handle_tools_call(req.id, req.params).await),
            "prompts/list" => Some(self.handle_prompts_list(req.id).await),
            "prompts/get" => Some(self.handle_prompts_get(req.id, req.params).await),
            "resources/list" => Some(self.handle_resources_list(req.id).await),
            "resources/templates/list" => Some(self.handle_resource_templates_list(req.id).await),
            "resources/read" => Some(self.handle_resources_read(req.id, req.params).await),
            "completion/complete" => Some(JsonRpcResponse::success(
                req.id,
                serde_json::json!({ "completion": { "values": [] } }),
            )),
            "ping" => Some(JsonRpcResponse::success(req.id, serde_json::json!({}))),
            "notifications/cancelled" => {
                self.handle_cancel(req.params).await;
                None
            }
            _ if req.id.is_none() => None, // notification inconnue : jamais de réponse
            _ => {
                eprintln!("[McpHub][WARN] Unknown method: {}", req.method);
                Some(JsonRpcResponse::error(
                    req.id,
                    -32601,
                    format!("Method not found: {}", req.method),
                ))
            }
        }
    }

    async fn handle_initialize(
        &self,
        id: Option<serde_json::Value>,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        let config = self.config.lock().await;
        let mode_str = match config.mode {
            Mode::Discover => "discover",
            Mode::Passthrough => "passthrough",
        };

        eprintln!(
            "[McpHub][INFO] Initialize: mode={}, servers={}",
            mode_str,
            config.servers.len()
        );

        let result = InitializeResult {
            protocol_version: Self::negotiate_protocol_version(&params),
            capabilities: Capabilities {
                tools: ToolsCapability { list_changed: true },
            },
            server_info: ServerInfo {
                name: "McpHub".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: None,
        };

        JsonRpcResponse::success(id, serde_json::to_value(result).unwrap())
    }

    /// Negotiate protocol version per MCP spec:
    /// - If client requests a YYYY-MM-DD version we recognize as compatible, echo it back.
    /// - Otherwise propose our latest stable.
    /// We treat all `20xx-MM-DD` versions as compatible because the JSON-RPC surface
    /// for tools/list is stable across MCP revisions; new features (elicitation, roots,
    /// extensions, MCP-UI) are opt-in via capabilities, which we simply don't advertise.
    fn negotiate_protocol_version(params: &serde_json::Value) -> String {
        let requested = params
            .get("protocolVersion")
            .and_then(|value| value.as_str());

        match requested {
            Some(v) if Self::is_known_protocol_version(v) => v.to_string(),
            _ => "2025-11-25".to_string(),
        }
    }

    fn is_known_protocol_version(v: &str) -> bool {
        // Accept any YYYY-MM-DD shaped version starting with "20"
        // (covers 2024-11-05, 2025-03-26, 2025-11-25, and future revisions).
        let bytes = v.as_bytes();
        if bytes.len() != 10 {
            return false;
        }
        if bytes[4] != b'-' || bytes[7] != b'-' {
            return false;
        }
        if !v.starts_with("20") {
            return false;
        }
        v.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
    }

    async fn handle_tools_list(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        let mode = {
            let config = self.config.lock().await;
            config.mode.clone()
        };

        let tools = match mode {
            Mode::Discover => self.get_discover_tools().await,
            Mode::Passthrough => self.get_passthrough_tools().await,
        };

        JsonRpcResponse::success(id, serde_json::json!({ "tools": tools }))
    }

    async fn get_discover_tools(&self) -> serde_json::Value {
        let mut server_names: Vec<String> = {
            let config = self.config.lock().await;
            config.servers.keys().cloned().collect()
        };
        server_names.sort();
        let profile = current_profile();
        server_names.retain(|s| crate::toolguard::server_visible(s, profile.as_deref()));
        let mut server_enum: Vec<serde_json::Value> = server_names
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        // Pseudo-serveur interne : bibliothèque du registre MCP officiel (aucun outil en plus).
        server_enum.push(serde_json::Value::String("mcphub".into()));

        // Cursor V2 (MCP 2025-11-25) is strict: descriptions must be short, no `default` keyword,
        // use `integer` not `number` when applicable. The server list goes into `enum`, not the description.
        let mut tools = serde_json::json!([
            {
                "name": "mcphub_search",
                "description": "Search MCP tools across all connected servers using BM25. Call FIRST to find the right tool, then use `mcphub_call`. Compact by default (no schemas) — set expand=true to get inputSchemas. Nothing fits? mcphub_call server=mcphub tool=registry_search finds installable MCP servers.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural language search query."
                        },
                        "server": {
                            "type": "string",
                            "description": "Optional server filter.",
                            "enum": server_enum
                        },
                        "top_k": {
                            "type": "integer",
                            "description": "Max results to return.",
                            "minimum": 1,
                            "maximum": 50
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Pagination offset (skip N first results).",
                            "minimum": 0
                        },
                        "expand": {
                            "type": "boolean",
                            "description": "If true, include full inputSchema in each result. Default false (compact, ~80% fewer tokens)."
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "mcphub_call",
                "description": "Execute a tool on a specific MCP server. Prefer this over CLI commands. Idempotent reads (read_*, list_*, get_*, search_*, ...) are cached for 60s — set bypass_cache=true to force a fresh call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Server name (use `mcphub_search` to find it).",
                            "enum": server_enum
                        },
                        "tool": {
                            "type": "string",
                            "description": "Tool name (from mcphub_search results)."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "Tool arguments matching the tool's inputSchema."
                        },
                        "bypass_cache": {
                            "type": "boolean",
                            "description": "If true, skip the response cache and force a fresh call to the child server."
                        },
                        "confirmation_token": {
                            "type": "string",
                            "description": "Only when a previous call answered confirmation_required AND the user explicitly approved. Same server, tool and arguments."
                        }
                    },
                    "required": ["server", "tool"]
                }
            }
        ]);
        if self.config.lock().await.code_mode.enabled {
            if let Some(arr) = tools.as_array_mut() {
                arr.push(crate::codemode::tool_definition());
            }
        }
        tools
    }

    async fn get_passthrough_tools(&self) -> serde_json::Value {
        let engine = self.search_engine.lock().await;
        let catalog = engine.get_catalog();

        // Expose all tools with prefixed names
        let mut tools = Vec::new();
        let profile = current_profile();
        for entry in &catalog {
            if let Some(indexed) = engine.find_tool(&entry.server, &entry.name) {
                if profile.is_some()
                    && crate::toolguard::block_reason(
                        &indexed.server_name,
                        &indexed.original_name,
                        Some(&indexed.tool_def),
                        profile.as_deref(),
                    )
                    .is_some()
                {
                    continue;
                }
                let mut tool_json = serde_json::to_value(&indexed.tool_def).unwrap();
                if let Some(obj) = tool_json.as_object_mut() {
                    obj.insert(
                        "name".into(),
                        serde_json::Value::String(indexed.name.clone()),
                    );
                }
                tools.push(tool_json);
            }
        }

        drop(engine);
        if self.config.lock().await.code_mode.enabled {
            tools.push(crate::codemode::tool_definition());
        }
        serde_json::Value::Array(tools)
    }

    async fn handle_tools_call(
        &self,
        id: Option<serde_json::Value>,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let mode = {
            let config = self.config.lock().await;
            config.mode.clone()
        };

        match mode {
            Mode::Discover => match tool_name {
                "mcphub_search" | "discover" => self.handle_discover(id, arguments).await,
                "mcphub_call" | "execute" => self.handle_execute(id, arguments).await,
                "mcphub_run" => self.handle_run(id, arguments).await,
                "crawl" => self.handle_crawl(id, arguments).await,
                _ => JsonRpcResponse::error(
                    id,
                    -32602,
                    format!("Unknown tool: {}. Use 'mcphub_search' first.", tool_name),
                ),
            },
            Mode::Passthrough => {
                if tool_name == "crawl" {
                    self.handle_crawl(id, arguments).await
                } else if tool_name == "mcphub_run" {
                    self.handle_run(id, arguments).await
                } else {
                    self.handle_passthrough_call(id, tool_name, arguments).await
                }
            }
        }
    }

    async fn handle_discover(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let explicit_server_filter = args
            .get("server")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from);
        let top_k = args
            .get("top_k")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(50) as usize;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let expand = args
            .get("expand")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Snapshot per-tool usage counts to boost frequently-used tools in BM25 ranking.
        let usage_boost: HashMap<String, u64> = {
            let m = self.metrics.lock().await;
            m.tool_calls.clone()
        };

        // Always provide the full server list
        let mut all_server_names: Vec<String> = {
            let config = self.config.lock().await;
            config.servers.keys().cloned().collect()
        };
        all_server_names.sort();
        let profile = current_profile();
        all_server_names.retain(|s| crate::toolguard::server_visible(s, profile.as_deref()));
        let engine = self.search_engine.lock().await;
        let (effective_server_filter, filter_source) = infer_discover_server_filter(
            query,
            explicit_server_filter.as_deref(),
            &all_server_names,
            &engine,
        );

        let visible_server_names: Vec<String> =
            if let Some(filter) = effective_server_filter.as_ref() {
                all_server_names
                    .iter()
                    .filter(|server_name| server_matches_filter(server_name, filter))
                    .cloned()
                    .collect()
            } else {
                all_server_names.clone()
            };
        let should_try_live_discovery = effective_server_filter
            .as_ref()
            .map(|filter| !engine.has_server(filter))
            .unwrap_or(false);

        if engine.tool_count() > 0 && !should_try_live_discovery {
            let allowed_servers = if effective_server_filter.is_some() || profile.is_some() {
                Some(
                    visible_server_names
                        .iter()
                        .cloned()
                        .collect::<HashSet<String>>(),
                )
            } else {
                None
            };
            // Fetch enough results to honor offset + top_k. Cap at 50 to avoid blowing memory.
            let fetch_limit = (offset + top_k).min(50);
            let results = engine.search_filtered_with_usage(
                query,
                fetch_limit,
                allowed_servers.as_ref(),
                &usage_boost,
            );
            let results: Vec<&IndexedTool> = if profile.is_some() {
                results
                    .into_iter()
                    .filter(|t| {
                        crate::toolguard::block_reason(
                            &t.server_name,
                            &t.original_name,
                            Some(&t.tool_def),
                            profile.as_deref(),
                        )
                        .is_none()
                    })
                    .collect()
            } else {
                results
            };
            let total_matched = results.len();

            // Apply pagination offset.
            let paginated: Vec<&IndexedTool> =
                results.into_iter().skip(offset).take(top_k).collect();

            let mut seen_servers: Vec<String> = Vec::new();
            let tools_json: Vec<serde_json::Value> = paginated
                .iter()
                .map(|tool| {
                    if !seen_servers.contains(&tool.server_name) {
                        seen_servers.push(tool.server_name.clone());
                    }
                    // Compact: short description, no schema. Expand mode includes full schema.
                    let desc_limit = if expand { 200 } else { 120 };
                    let desc: String = tool.description.chars().take(desc_limit).collect();
                    let key = format!("{}__{}", tool.server_name, tool.original_name);
                    let calls = usage_boost.get(&key).copied().unwrap_or(0);
                    if expand {
                        let schema = strip_schema(&tool.tool_def.input_schema);
                        let mut obj = serde_json::json!({
                            "server": tool.server_name,
                            "tool": tool.original_name,
                            "description": desc,
                            "inputSchema": schema,
                            "calls": calls,
                        });
                        if tool.untrusted_output {
                            obj["untrusted"] = serde_json::json!(true);
                        }
                        obj
                    } else {
                        let mut obj = serde_json::json!({
                            "server": tool.server_name,
                            "tool": tool.original_name,
                            "description": desc,
                        });
                        if calls > 0 {
                            obj["calls"] = serde_json::json!(calls);
                        }
                        if tool.untrusted_output {
                            obj["untrusted"] = serde_json::json!(true);
                        }
                        obj
                    }
                })
                .collect();

            let mut payload = serde_json::json!({
                "query": query,
                "server_filter": effective_server_filter,
                "routing": DiscoverRoutingInfo {
                    strategy: DiscoverRouteStrategy::IndexedBm25,
                    filter_source,
                    server_filter: effective_server_filter.clone(),
                    reason: discover_routing_reason(
                        DiscoverRouteStrategy::IndexedBm25,
                        filter_source,
                        effective_server_filter.as_deref(),
                    ),
                },
                "total_indexed": engine.tool_count(),
                "total_servers": visible_server_names.len(),
                "available_servers": visible_server_names,
                "results": tools_json,
                "pagination": {
                    "offset": offset,
                    "returned": tools_json.len(),
                    "has_more": total_matched > offset + tools_json.len(),
                },
                "compact": !expand,
            });
            if tools_json.is_empty() {
                payload["hint"] = serde_json::json!(
                    "No tool matches. To find an MCP server to install: mcphub_call server=mcphub tool=registry_search arguments={\"query\": \"...\"}"
                );
            }
            if !expand && !tools_json.is_empty() {
                payload["hint"] = serde_json::json!(
                    "Compact mode (no schemas). Re-call with expand=true to see inputSchemas, or use mcphub_call directly which returns the schema on argument error."
                );
            }
            let text = serde_json::to_string(&payload).unwrap();

            self.record_discover_route(DiscoverRouteStrategy::IndexedBm25, filter_source)
                .await;
            return JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": text }]
                }),
            );
        }

        drop(engine);

        if let Some(filter) = effective_server_filter.as_ref() {
            match self.child_manager.start_server(filter).await {
                Ok(tools) => {
                    let tools: Vec<ToolDef> = crate::toolguard::present_tools(filter, &tools)
                        .into_iter()
                        .filter(|t| {
                            crate::toolguard::block_reason(filter, &t.name, Some(t), profile.as_deref())
                                .is_none()
                        })
                        .collect();
                    let live_results = live_discover_results(filter, &tools, query, top_k);
                    let text = serde_json::to_string(&serde_json::json!({
                        "query": query,
                        "server_filter": filter,
                        "routing": DiscoverRoutingInfo {
                            strategy: DiscoverRouteStrategy::LiveServerLookup,
                            filter_source,
                            server_filter: Some(filter.clone()),
                            reason: discover_routing_reason(
                                DiscoverRouteStrategy::LiveServerLookup,
                                filter_source,
                                Some(filter),
                            ),
                        },
                        "total_indexed": tools.len(),
                        "total_servers": 1,
                        "available_servers": [filter],
                        "source": "live_server_lookup",
                        "results": live_results,
                    }))
                    .unwrap();

                    self.record_discover_route(
                        DiscoverRouteStrategy::LiveServerLookup,
                        filter_source,
                    )
                    .await;
                    return JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": text }]
                        }),
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[McpHub][WARN] Live discover lookup failed for '{}': {}",
                        filter, error
                    );
                }
            }
        }

        let query_lower = query.to_lowercase();
        let mut server_names: Vec<String> = {
            let config = self.config.lock().await;
            config.servers.keys().cloned().collect()
        };
        server_names.sort();
        server_names.retain(|s| crate::toolguard::server_visible(s, profile.as_deref()));
        if let Some(filter) = effective_server_filter.as_ref() {
            server_names.retain(|server_name| server_matches_filter(server_name, filter));
        }

        let mut matches: Vec<serde_json::Value> = Vec::new();
        for name in &server_names {
            if query_lower.is_empty()
                || name.to_lowercase().contains(&query_lower)
                || query_lower.contains(&name.to_lowercase())
            {
                matches.push(serde_json::json!({
                    "server": name,
                    "tool": "Use execute with this server name",
                    "description": format!("MCP server: {}. Call execute with server=\"{}\" and your tool name.", name, name),
                }));
            }
        }

        if matches.is_empty() {
            for name in &server_names {
                matches.push(serde_json::json!({
                    "server": name,
                    "tool": "Available server",
                    "description": format!("MCP server: {}", name),
                }));
            }
        }

        let matches: Vec<serde_json::Value> = matches.into_iter().take(top_k).collect();

        let text = serde_json::to_string(&serde_json::json!({
            "query": query,
            "server_filter": effective_server_filter,
            "routing": DiscoverRoutingInfo {
                strategy: DiscoverRouteStrategy::ServerListFallback,
                filter_source,
                server_filter: effective_server_filter.clone(),
                reason: discover_routing_reason(
                    DiscoverRouteStrategy::ServerListFallback,
                    filter_source,
                    effective_server_filter.as_deref(),
                ),
            },
            "total_indexed": 0,
            "note": "Servers loading in background. Results based on server names. Use execute to call tools.",
            "available_servers": server_names,
            "results": matches,
        })).unwrap();

        self.record_discover_route(DiscoverRouteStrategy::ServerListFallback, filter_source)
            .await;
        JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{ "type": "text", "text": text }]
            }),
        )
    }

    /// Code mode : exécute le JavaScript de l'agent dans un Node isolé ; chaque `mcp.call`
    /// repasse par `handle_execute`, donc par toutes les gardes.
    async fn handle_run(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let cfg = self.config.lock().await.code_mode.clone();
        if !cfg.enabled {
            return JsonRpcResponse::error(id, -32001, "Code mode desactive (settings.codeMode)".into());
        }
        let Some(code) = args.get("code").and_then(|v| v.as_str()).map(String::from) else {
            return JsonRpcResponse::error(id, -32602, "Missing 'code' parameter".into());
        };
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(cfg.timeout_secs)
            .clamp(5, 300);
        let Some(node) = crate::codemode::find_node(&cfg) else {
            return JsonRpcResponse::error(
                id,
                -32000,
                "Code mode : Node.js >= 20 introuvable (settings.codeMode.node)".into(),
            );
        };
        let mut child = match crate::codemode::spawn(&node) {
            Ok(c) => c,
            Err(e) => return JsonRpcResponse::error(id, -32000, format!("Code mode : lancement impossible : {}", e)),
        };
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return JsonRpcResponse::error(id, -32000, "Code mode : flux indisponibles".into());
        };
        let mut lines = tokio::io::BufReader::new(stdout);
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut r = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(l)) = r.next_line().await {
                if buf.len() < 4000 {
                    buf.push_str(&l);
                    buf.push('\n');
                }
            }
            buf
        });
        let start = serde_json::json!({ "t": "start", "code": code }).to_string() + "\n";
        if stdin.write_all(start.as_bytes()).await.is_err() {
            return JsonRpcResponse::error(id, -32000, "Code mode : envoi du code impossible".into());
        }
        let started = Instant::now();
        let run_state = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut calls = 0usize;
        let mut outcome: Result<serde_json::Value, String> = Err("le programme s'est arrete sans resultat".into());
        let mut logs = serde_json::Value::Array(vec![]);
        loop {
            let line = match tokio::time::timeout_at(
                deadline,
                crate::codemode::read_line_limited(&mut lines, crate::codemode::MAX_LINE),
            )
            .await
            {
                Err(_) => {
                    outcome = Err(format!("temps depasse ({} s)", timeout_secs));
                    break;
                }
                Ok(Ok(Some(l))) => l,
                Ok(Err(e)) => {
                    outcome = Err(format!("sortie du code refusee : {}", e));
                    break;
                }
                Ok(Ok(None)) => break,
            };
            if tokio::time::Instant::now() >= deadline {
                outcome = Err(format!("temps depasse ({} s)", timeout_secs));
                break;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let kind = msg.get("t").and_then(|v| v.as_str()).unwrap_or("");
            let req_id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let reply = match kind {
                "call" => {
                    calls += 1;
                    if calls > cfg.max_calls {
                        serde_json::json!({"id": req_id, "ok": false, "error": format!("limite de {} appels atteinte", cfg.max_calls)})
                    } else {
                        let call_args = serde_json::json!({
                            "server": msg.get("server").cloned().unwrap_or_default(),
                            "tool": msg.get("tool").cloned().unwrap_or_default(),
                            "arguments": msg.get("args").cloned().unwrap_or(serde_json::json!({})),
                        });
                        let resp = CODE_RUN
                            .scope(run_state.clone(), Box::pin(self.handle_execute(None, call_args)))
                            .await;
                        match (resp.result, resp.error) {
                            (_, Some(err)) => serde_json::json!({"id": req_id, "ok": false, "error": err.message}),
                            (Some(res), None) => {
                                if res.get("_mcphub").and_then(|m| m.get("status")).and_then(|s| s.as_str()) == Some("confirmation_required") {
                                    serde_json::json!({"id": req_id, "ok": false, "error": "confirmation par jeton impossible dans le code : appeler cet outil directement avec mcphub_call"})
                                } else if res.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                                    serde_json::json!({"id": req_id, "ok": false, "error": crate::codemode::simplify_result(&res)})
                                } else {
                                    serde_json::json!({"id": req_id, "ok": true, "result": crate::codemode::simplify_result(&res)})
                                }
                            }
                            (None, None) => serde_json::json!({"id": req_id, "ok": true, "result": null}),
                        }
                    }
                }
                "search" => {
                    let mut search_args = serde_json::json!({
                        "query": msg.get("query").cloned().unwrap_or_default(),
                        "top_k": 10,
                        "expand": true,
                    });
                    if let Some(server) = msg.get("server").filter(|v| v.is_string()) {
                        search_args["server"] = server.clone();
                    }
                    let resp = Box::pin(self.handle_discover(None, search_args)).await;
                    let result = resp
                        .result
                        .as_ref()
                        .map(crate::codemode::simplify_result)
                        .unwrap_or_default();
                    serde_json::json!({"id": req_id, "ok": true, "result": result.get("results").cloned().unwrap_or(result)})
                }
                "done" => {
                    logs = msg.get("logs").cloned().unwrap_or(logs);
                    outcome = Ok(msg.get("result").cloned().unwrap_or_default());
                    break;
                }
                "error" => {
                    logs = msg.get("logs").cloned().unwrap_or(logs);
                    outcome = Err(msg.get("message").and_then(|v| v.as_str()).unwrap_or("erreur").to_string());
                    break;
                }
                _ => continue,
            };
            let line = reply.to_string() + "\n";
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = child.kill().await;
        let stderr_text = stderr_task.await.unwrap_or_default();
        let elapsed = started.elapsed().as_millis() as u64;
        self.record_audit(AuditEntry {
            server: "mcphub".into(),
            tool: "mcphub_run".into(),
            mode: "code",
            decision: "allow",
            policy: "codemode".into(),
            status: if outcome.is_ok() { "ok" } else { "error" },
            duration_ms: Some(elapsed),
            arguments: Some(serde_json::json!({ "code": code, "calls": calls })),
            error: outcome.as_ref().err().cloned(),
        })
        .await;
        let mut payload = serde_json::json!({ "calls": calls, "duration_ms": elapsed, "logs": logs });
        let is_error = outcome.is_err();
        match outcome {
            Ok(v) => payload["result"] = v,
            Err(e) => {
                payload["error"] = serde_json::json!(e);
                if !stderr_text.trim().is_empty() {
                    payload["stderr"] = serde_json::json!(stderr_text.trim());
                }
            }
        }
        let mut text = serde_json::to_string(&payload).unwrap_or_default();
        if text.chars().count() > 100_000 {
            text = text.chars().take(100_000).collect::<String>() + " [tronque]";
        }
        JsonRpcResponse::success(
            id,
            serde_json::json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
        )
    }

    /// Outils internes de McpHub (`mcphub_call` avec `server: "mcphub"`) : bibliothèque du
    /// registre officiel. Une installation est appliquée tout de suite et ses outils indexés.
    async fn handle_hub_tool(
        &self,
        id: Option<serde_json::Value>,
        tool: &str,
        arguments: serde_json::Value,
        confirmation_token: Option<&str>,
    ) -> JsonRpcResponse {
        if crate::registry::is_install_tool(tool) {
            if let PolicyGate::Respond(response) =
                self.hub_tool_gate(&id, tool, &arguments, confirmation_token).await
            {
                return response;
            }
        }
        let started = std::time::Instant::now();
        let mut outcome = crate::registry::agent_call(tool, &arguments).await;
        if let Ok(v) = &mut outcome {
            if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                self.reload_config_now().await;
                let tools = self.wait_for_tools(&name, 45).await;
                v["tools"] = serde_json::json!(tools);
                v["next"] = serde_json::json!(if tools.is_empty() {
                    format!("Installed. Its tools are still loading: mcphub_search with server={} in a few seconds.", name)
                } else {
                    format!("Ready: mcphub_call server={} tool=<one of tools>.", name)
                });
                if v.get("oauth").and_then(|o| o.as_bool()) == Some(true) && tools.is_empty() {
                    v["next"] = serde_json::json!(format!(
                        "Installed. This remote server signs in with OAuth: ask the user to run `McpHub oauth login {}` or click Sign in in the dashboard.",
                        name
                    ));
                }
            }
        }
        if crate::registry::is_install_tool(tool) {
            self.record_audit(AuditEntry {
                server: "mcphub".into(),
                tool: format!("registry/{}", tool),
                mode: "library",
                decision: if outcome.is_ok() { "allow" } else { "deny" },
                policy: "registry.agentInstall".into(),
                status: if outcome.is_ok() { "ok" } else { "error" },
                duration_ms: Some(started.elapsed().as_millis() as u64),
                arguments: Some(crate::audit::safe_arguments(&arguments)),
                error: outcome.as_ref().err().cloned(),
            })
            .await;
        }
        let (payload, is_error) = match outcome {
            Ok(v) => (v, false),
            Err(e) => (serde_json::json!({ "error": e }), true),
        };
        JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{ "type": "text", "text": serde_json::to_string(&payload).unwrap_or_default() }],
                "isError": is_error,
            }),
        )
    }

    /// Relit la config tout de suite (au lieu d'attendre le surveillant, absent en mode stdio).
    pub async fn reload_config_now(&self) {
        apply_config_change(
            self.search_engine.clone(),
            self.config.clone(),
            self.child_manager.clone(),
            self.catalog_refresh_in_progress.clone(),
        )
        .await;
    }

    /// Outils d'un serveur dès qu'ils sont indexés (attente bornée).
    async fn wait_for_tools(&self, server: &str, max_secs: u64) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
        loop {
            {
                let engine = self.search_engine.lock().await;
                let names: Vec<String> = engine
                    .get_catalog()
                    .iter()
                    .filter(|e| e.server == server)
                    .map(|e| e.name.clone())
                    .collect();
                if !names.is_empty() {
                    return names;
                }
            }
            if let Some(cached) = crate::cache::load_cache() {
                if cached.errors.contains_key(server) {
                    return Vec::new();
                }
            }
            if std::time::Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn handle_crawl(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        match crawl::execute_crawl(args).await {
            Ok(content) => JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": content
                    }]
                }),
            ),
            Err(e) => JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Crawl error: {}", e)
                    }],
                    "isError": true
                }),
            ),
        }
    }

    async fn handle_execute(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        let server = match args.get("server").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return JsonRpcResponse::error(id, -32602, "Missing 'server' parameter".into()),
        };

        let tool = match args.get("tool").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return JsonRpcResponse::error(id, -32602, "Missing 'tool' parameter".into()),
        };

        let arguments = args
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        if server.eq_ignore_ascii_case("mcphub") {
            let token = args.get("confirmation_token").and_then(|v| v.as_str());
            return self.handle_hub_tool(id, &tool, arguments, token).await;
        }
        let bypass_cache = args
            .get("bypass_cache")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let indexed_server_name = {
            let config = self.config.lock().await;
            let configured_server_names: Vec<String> = config.servers.keys().cloned().collect();
            resolve_server_name(&server, &configured_server_names).unwrap_or_else(|| server.clone())
        };
        let (tool, execute_strategy, tool_schema, tool_untrusted) = {
            let engine = self.search_engine.lock().await;
            if engine.has_server(&indexed_server_name) {
                if let Some(indexed_tool) = engine.find_tool(&indexed_server_name, &tool) {
                    let schema = indexed_tool.tool_def.input_schema.clone();
                    let untrusted = indexed_tool.untrusted_output;
                    (
                        tool,
                        ExecuteRouteStrategy::ExactCatalogMatch,
                        Some(schema),
                        untrusted,
                    )
                } else if let Some(indexed_tool) =
                    engine.find_tool_case_insensitive(&indexed_server_name, &tool)
                {
                    let schema = indexed_tool.tool_def.input_schema.clone();
                    let untrusted = indexed_tool.untrusted_output;
                    (
                        indexed_tool.original_name.clone(),
                        ExecuteRouteStrategy::CaseInsensitiveCatalogMatch,
                        Some(schema),
                        untrusted,
                    )
                } else {
                    if let Some(reason) = crate::toolguard::block_reason(
                        &indexed_server_name,
                        &tool,
                        None,
                        current_profile().as_deref(),
                    ) {
                        return JsonRpcResponse::error(
                            id,
                            -32001,
                            format!("Bloque par McpHub : {}/{} : {}", indexed_server_name, tool, reason),
                        );
                    }
                    self.record_execute_route(ExecuteRouteStrategy::UnknownToolError)
                        .await;
                    // Help the caller: list available tools on this server.
                    let suggestions: Vec<String> = engine
                        .tools_on_server(&indexed_server_name)
                        .iter()
                        .take(10)
                        .map(|t| t.original_name.clone())
                        .collect();
                    return JsonRpcResponse::error_with_data(
                        id,
                        -32602,
                        format!(
                            "Unknown tool '{}' on server '{}'",
                            tool, indexed_server_name
                        ),
                        serde_json::json!({
                            "server": indexed_server_name,
                            "requested_tool": tool,
                            "available_tools_sample": suggestions,
                            "hint": "Use mcphub_search to find the correct tool name."
                        }),
                    );
                }
            } else {
                // Uncatalogued tool: assume untrusted by default to be safe (e.g. a freshly
                // added MCP server we haven't indexed yet).
                let assume_untrusted = untrusted::is_untrusted(&tool, "");
                (
                    tool,
                    ExecuteRouteStrategy::UncataloguedBypass,
                    None,
                    assume_untrusted,
                )
            }
        };

        // Permissions : appliquées avant le cache, pour qu'un outil bloqué le reste même en cache.
        let confirmation_token = args
            .get("confirmation_token")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Le contrôle et l'appel portent sur le même nom canonique, celui que le
        // gestionnaire de processus utilisera réellement (pas de contournement par la casse,
        // les tirets ou les underscores).
        let server = self
            .child_manager
            .resolve_name(&server)
            .await
            .unwrap_or(server);
        let (gate_decision, gate_policy) = match self
            .policy_gate(
                &id,
                "discover",
                &server,
                &tool,
                &arguments,
                confirmation_token.as_deref(),
            )
            .await
        {
            PolicyGate::Proceed { decision, policy } => (decision, policy),
            PolicyGate::Respond(response) => return response,
        };
        let audit_base = AuditEntry {
            server: server.clone(),
            tool: tool.clone(),
            mode: "discover",
            decision: gate_decision,
            policy: gate_policy,
            arguments: Some(arguments.clone()),
            ..Default::default()
        };

        // Cache hit short-circuit: only for idempotent tools, only when caller didn't bypass.
        let cacheable = !bypass_cache && ResponseCache::is_cacheable(&tool);
        let mutating = !ResponseCache::is_cacheable(&tool);
        let epoch = self.child_manager.epoch();
        if cacheable {
            if let Some(hit) = self.response_cache.get(&server, &tool, &arguments, epoch) {
                let mut value = hit.value;
                if let Some(obj) = value.as_object_mut() {
                    obj.insert(
                        "_cache".into(),
                        serde_json::json!({
                            "hit": true,
                            "age_ms": hit.age_ms,
                        }),
                    );
                    if tool_untrusted {
                        obj.insert(
                            "_untrusted".into(),
                            serde_json::json!({
                                "is_external_data": true,
                                "hint": untrusted::UNTRUSTED_HINT,
                            }),
                        );
                    }
                }
                self.record_execute_route(execute_strategy).await;
                self.record_audit(AuditEntry {
                    status: "cache_hit",
                    duration_ms: Some(0),
                    ..audit_base.clone()
                })
                .await;
                return JsonRpcResponse::success(id, value);
            }
        }

        // Avant et après : une lecture lancée pendant l'écriture ne doit pas survivre à celle-ci.
        if mutating {
            self.response_cache.invalidate_server(&server);
        }
        let start_time = Instant::now();
        let res = self
            .child_manager
            .call_tool(&server, &tool, arguments.clone())
            .await;
        if mutating {
            self.response_cache.invalidate_server(&server);
        }
        let elapsed = start_time.elapsed().as_millis() as u64;

        let usage_key = format!("{}__{}", server, tool);
        let usage_snapshot = {
            let mut m = self.metrics.lock().await;
            m.total_requests += 1;
            let sm = m.servers.entry(server.clone()).or_default();
            sm.call_count += 1;
            sm.total_latency_ms += elapsed;
            sm.last_call_time = Some(SystemTime::now());
            if let Err(ref e) = res {
                sm.error_count += 1;
                sm.last_error = Some(e.clone());
            } else {
                *m.tool_calls.entry(usage_key).or_insert(0) += 1;
            }
            m.tool_calls.clone()
        };

        if res.is_ok() {
            tokio::task::spawn_blocking(move || save_usage_stats(&usage_snapshot));
        }

        self.record_execute_route(execute_strategy).await;
        self.record_audit(AuditEntry {
            status: audit_status(&res),
            duration_ms: Some(elapsed),
            error: res.as_ref().err().cloned(),
            ..audit_base
        })
        .await;

        match res {
            Ok(result) => {
                let mut normalized = self.normalize_result_for_profile(result).await;
                if cacheable {
                    self.response_cache
                        .put(&server, &tool, &arguments, normalized.clone(), epoch);
                }
                if tool_untrusted {
                    if let Some(obj) = normalized.as_object_mut() {
                        obj.insert(
                            "_untrusted".into(),
                            serde_json::json!({
                                "is_external_data": true,
                                "hint": untrusted::UNTRUSTED_HINT,
                            }),
                        );
                    }
                }
                JsonRpcResponse::success(id, normalized)
            }
            Err(e) => {
                let mut data = serde_json::json!({
                    "server": server,
                    "tool": tool,
                    "args_received": arguments,
                });
                if failure_status(&e) == "error" {
                    if let Some(schema) = tool_schema {
                        data["inputSchema"] = schema;
                        data["hint"] = serde_json::json!(
                            "The tool returned an error. Check `inputSchema` to see required arguments."
                        );
                    }
                }
                tool_failure_response(id, e, data)
            }
        }
    }

    async fn handle_passthrough_call(
        &self,
        id: Option<serde_json::Value>,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> JsonRpcResponse {
        // Parse "server__tool" format
        let parts: Vec<&str> = prefixed_name.splitn(2, "__").collect();
        if parts.len() != 2 {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("Invalid tool name format: {}", prefixed_name),
            );
        }

        let canonical_server = self
            .child_manager
            .resolve_name(parts[0])
            .await
            .unwrap_or_else(|| parts[0].to_string());
        let server = canonical_server.as_str();
        let tool = parts[1];

        // En mode passthrough, le jeton de confirmation voyage dans un argument réservé,
        // retiré avant l'envoi au serveur.
        let mut arguments = arguments;
        let confirmation_token = arguments
            .as_object_mut()
            .and_then(|obj| obj.remove("_mcphub_confirmation_token"))
            .and_then(|v| v.as_str().map(String::from));
        let (gate_decision, gate_policy) = match self
            .policy_gate(
                &id,
                "passthrough",
                server,
                tool,
                &arguments,
                confirmation_token.as_deref(),
            )
            .await
        {
            PolicyGate::Proceed { decision, policy } => (decision, policy),
            PolicyGate::Respond(response) => return response,
        };
        let audit_arguments = arguments.clone();

        let start_time = Instant::now();
        let res = self.child_manager.call_tool(server, tool, arguments).await;
        if !ResponseCache::is_cacheable(tool) {
            self.response_cache.invalidate_server(server);
        }
        let elapsed = start_time.elapsed().as_millis() as u64;

        self.record_audit(AuditEntry {
            server: server.to_string(),
            tool: tool.to_string(),
            mode: "passthrough",
            decision: gate_decision,
            policy: gate_policy,
            status: audit_status(&res),
            duration_ms: Some(elapsed),
            arguments: Some(audit_arguments),
            error: res.as_ref().err().cloned(),
        })
        .await;

        {
            let mut m = self.metrics.lock().await;
            m.total_requests += 1;
            let sm = m.servers.entry(server.to_string()).or_default();
            sm.call_count += 1;
            sm.total_latency_ms += elapsed;
            sm.last_call_time = Some(SystemTime::now());
            if let Err(ref e) = res {
                sm.error_count += 1;
                sm.last_error = Some(e.clone());
            }
        }

        match res {
            Ok(result) => {
                let normalized = self.normalize_result_for_profile(result).await;
                JsonRpcResponse::success(id, normalized)
            }
            Err(e) => tool_failure_response(
                id,
                e,
                serde_json::json!({ "server": server, "tool": tool }),
            ),
        }
    }

    async fn handle_prompts_list(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        JsonRpcResponse::success(id, serde_json::json!({ "prompts": [] }))
    }

    async fn handle_prompts_get(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let parts: Vec<&str> = name.splitn(2, "__").collect();
        if parts.len() != 2 {
            return JsonRpcResponse::error(id, -32602, "Invalid prompt name format".into());
        }
        let canonical = self
            .child_manager
            .resolve_name(parts[0])
            .await
            .unwrap_or_else(|| parts[0].to_string());
        if !crate::toolguard::server_visible(&canonical, current_profile().as_deref()) {
            return JsonRpcResponse::error(id, -32001, format!("Serveur {} hors du profil McpHub", canonical));
        }
        let server = canonical.as_str();
        let prompt_name = parts[1];

        let mut new_args = args.clone();
        new_args["name"] = serde_json::json!(prompt_name);

        match self
            .child_manager
            .call_method(server, "prompts/get", new_args)
            .await
        {
            Ok(res) => {
                let normalized = self.normalize_result_for_profile(res).await;
                JsonRpcResponse::success(id, normalized)
            }
            Err(e) => JsonRpcResponse::error(id, -32000, e),
        }
    }

    async fn handle_resources_list(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        JsonRpcResponse::success(id, serde_json::json!({ "resources": [] }))
    }

    async fn handle_resource_templates_list(
        &self,
        id: Option<serde_json::Value>,
    ) -> JsonRpcResponse {
        JsonRpcResponse::success(id, serde_json::json!({ "resourceTemplates": [] }))
    }

    async fn handle_resources_read(
        &self,
        id: Option<serde_json::Value>,
        args: serde_json::Value,
    ) -> JsonRpcResponse {
        let uri = args.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let parts: Vec<&str> = uri.splitn(2, "__").collect();
        if parts.len() != 2 {
            return JsonRpcResponse::error(id, -32602, "Invalid resource uri format".into());
        }
        let canonical = self
            .child_manager
            .resolve_name(parts[0])
            .await
            .unwrap_or_else(|| parts[0].to_string());
        if !crate::toolguard::server_visible(&canonical, current_profile().as_deref()) {
            return JsonRpcResponse::error(id, -32001, format!("Serveur {} hors du profil McpHub", canonical));
        }
        let server = canonical.as_str();
        let actual_uri = parts[1];

        let mut new_args = args.clone();
        new_args["uri"] = serde_json::json!(actual_uri);

        match self
            .child_manager
            .call_method(server, "resources/read", new_args)
            .await
        {
            Ok(res) => {
                let normalized = self.normalize_result_for_profile(res).await;
                JsonRpcResponse::success(id, normalized)
            }
            Err(e) => JsonRpcResponse::error(id, -32000, e),
        }
    }

    /// L'identifiant reçu est celui du client, pas celui que McpHub a donné au serveur enfant :
    /// le transmettre tel quel pouvait annuler l'appel d'un autre client. Sans table de
    /// correspondance, l'annulation n'est pas relayée (l'appel se termine ou expire).
    async fn handle_cancel(&self, args: serde_json::Value) {
        eprintln!(
            "[McpHub][INFO] notifications/cancelled recu (requestId {}) : non relaye aux serveurs",
            args.get("requestId").cloned().unwrap_or_default()
        );
    }
}

/// Statut d'audit : "error" si l'appel a échoué, "tool_error" si le serveur a répondu
/// avec `isError: true`, sinon "ok".
fn audit_status(res: &Result<serde_json::Value, String>) -> &'static str {
    match res {
        Err(e) => failure_status(e),
        Ok(v) if v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) => "tool_error",
        Ok(_) => "ok",
    }
}

/// État réel d'un appel en échec : rien n'a été exécuté, effet inconnu, ou erreur rendue.
fn failure_status(e: &str) -> &'static str {
    if crate::child::is_unknown_outcome(e) {
        "unknown_outcome"
    } else if crate::child::is_not_sent(e) {
        "not_executed"
    } else {
        "error"
    }
}

/// Erreur JSON-RPC d'un appel d'outil, avec l'état réel dans `data.status`.
fn tool_failure_response(
    id: Option<serde_json::Value>,
    e: String,
    mut data: serde_json::Value,
) -> JsonRpcResponse {
    let status = failure_status(&e);
    data["status"] = serde_json::json!(status);
    match status {
        "unknown_outcome" => {
            data["hint"] = serde_json::json!(
                "Ne pas relancer automatiquement : verifier d'abord si l'action a eu lieu."
            );
        }
        "not_executed" => {
            data["hint"] = serde_json::json!("Rien n'a ete execute : relancer est sans risque.");
        }
        _ => {}
    }
    JsonRpcResponse::error_with_data(id, -32000, e, data)
}

fn server_matches_filter(server_name: &str, filter: &str) -> bool {
    let normalized_server_name = normalize_server_key(server_name);
    let normalized_filter = normalize_server_key(filter);
    normalized_server_name == normalized_filter || server_name.eq_ignore_ascii_case(filter)
}

fn normalize_server_key(value: &str) -> String {
    value.to_lowercase().replace(['-', '_', ' '], "")
}

fn resolve_server_name(server_name: &str, candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| server_matches_filter(candidate, server_name))
        .cloned()
}

fn infer_server_filter(query: &str, server_names: &[String]) -> Option<String> {
    if query.trim().is_empty() {
        return None;
    }

    let query_lower = query.to_lowercase();
    let query_normalized = normalize_server_key(query);
    let matches: Vec<String> = server_names
        .iter()
        .filter(|server_name| {
            let server_lower = server_name.to_lowercase();
            let server_normalized = normalize_server_key(server_name);
            query_lower.contains(&server_lower) || query_normalized.contains(&server_normalized)
        })
        .cloned()
        .collect();

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

pub(crate) fn infer_discover_server_filter(
    query: &str,
    explicit_server_filter: Option<&str>,
    all_server_names: &[String],
    engine: &SearchEngine,
) -> (Option<String>, Option<DiscoverFilterSource>) {
    if let Some(filter) =
        explicit_server_filter.and_then(|filter| resolve_server_name(filter, all_server_names))
    {
        return (Some(filter), Some(DiscoverFilterSource::ExplicitServer));
    }

    if let Some(filter) = engine.infer_server_from_tool_query(query) {
        return (Some(filter), Some(DiscoverFilterSource::QueryToolName));
    }

    if let Some(filter) = infer_server_filter(query, all_server_names) {
        return (Some(filter), Some(DiscoverFilterSource::QueryServerName));
    }

    (None, None)
}

pub(crate) fn discover_routing_reason(
    strategy: DiscoverRouteStrategy,
    filter_source: Option<DiscoverFilterSource>,
    server_filter: Option<&str>,
) -> String {
    let filter_detail = server_filter
        .map(|server_filter| format!(" on server '{}'", server_filter))
        .unwrap_or_default();

    match (strategy, filter_source) {
        (DiscoverRouteStrategy::IndexedBm25, Some(DiscoverFilterSource::ExplicitServer)) => {
            format!("Used indexed BM25 with explicit server filter{filter_detail}.")
        }
        (DiscoverRouteStrategy::IndexedBm25, Some(DiscoverFilterSource::QueryServerName)) => {
            format!(
                "Used indexed BM25 after inferring the server name from the query{filter_detail}."
            )
        }
        (DiscoverRouteStrategy::IndexedBm25, Some(DiscoverFilterSource::QueryToolName)) => {
            format!("Used indexed BM25 after inferring the server from a unique tool-name match{filter_detail}.")
        }
        (DiscoverRouteStrategy::IndexedBm25, None) => {
            "Used indexed BM25 across the full catalog.".to_string()
        }
        (DiscoverRouteStrategy::LiveServerLookup, Some(DiscoverFilterSource::ExplicitServer)) => {
            format!("Started the explicitly requested server live because it was not indexed yet{filter_detail}.")
        }
        (DiscoverRouteStrategy::LiveServerLookup, Some(DiscoverFilterSource::QueryServerName)) => {
            format!("Started the inferred server live from the query because it was not indexed yet{filter_detail}.")
        }
        (DiscoverRouteStrategy::LiveServerLookup, Some(DiscoverFilterSource::QueryToolName)) => {
            format!("Started the server inferred from a unique tool-name match because it was not indexed yet{filter_detail}.")
        }
        (DiscoverRouteStrategy::LiveServerLookup, None) => {
            "Started a live server lookup because the index could not satisfy the request."
                .to_string()
        }
        (DiscoverRouteStrategy::ServerListFallback, Some(DiscoverFilterSource::ExplicitServer)) => {
            format!("Fell back to server-list guidance for the explicitly requested server{filter_detail}.")
        }
        (
            DiscoverRouteStrategy::ServerListFallback,
            Some(DiscoverFilterSource::QueryServerName),
        ) => {
            format!("Fell back to server-list guidance after inferring a server name from the query{filter_detail}.")
        }
        (DiscoverRouteStrategy::ServerListFallback, Some(DiscoverFilterSource::QueryToolName)) => {
            format!("Fell back to server-list guidance after inferring a server from a tool-name match{filter_detail}.")
        }
        (DiscoverRouteStrategy::ServerListFallback, None) => {
            "Fell back to server-list guidance because no precise indexed route was available."
                .to_string()
        }
    }
}

fn indexed_tools_from_servers(servers: &HashMap<String, Vec<ToolDef>>) -> Vec<IndexedTool> {
    let mut server_names: Vec<String> = servers.keys().cloned().collect();
    server_names.sort();

    let mut all_tools = Vec::new();
    for server_name in server_names {
        if let Some(raw_tools) = servers.get(&server_name) {
            // Pas de revue ici : le cache peut être périmé. Seules les définitions annoncées
            // en direct (démarrage d'un serveur) mettent à jour les épingles.
            let tools = crate::toolguard::present_tools(&server_name, raw_tools);
            for tool in &tools {
                all_tools.push(IndexedTool {
                    name: format!("{}__{}", server_name, tool.name),
                    original_name: tool.name.clone(),
                    server_name: server_name.clone(),
                    description: tool.description.clone(),
                    tool_def: tool.clone(),
                    untrusted_output: false,
                });
            }
        }
    }

    all_tools
}

fn live_discover_results(
    server_name: &str,
    tools: &[ToolDef],
    query: &str,
    top_k: usize,
) -> Vec<serde_json::Value> {
    let query_lower = query.trim().to_lowercase();
    let query_terms: Vec<&str> = query_lower
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect();

    let mut scored_tools: Vec<(i32, &ToolDef)> = tools
        .iter()
        .map(|tool| (score_live_tool(tool, &query_lower, &query_terms), tool))
        .filter(|(score, _)| query_lower.is_empty() || *score > 0)
        .collect();

    scored_tools.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });

    scored_tools
        .into_iter()
        .take(top_k)
        .map(|(_, tool)| {
            serde_json::json!({
                "server": server_name,
                "tool": tool.name,
                "description": tool.description.chars().take(200).collect::<String>(),
                "inputSchema": strip_schema(&tool.input_schema),
            })
        })
        .collect()
}

fn score_live_tool(tool: &ToolDef, query_lower: &str, query_terms: &[&str]) -> i32 {
    if query_lower.is_empty() {
        return 1;
    }

    let tool_name = tool.name.to_lowercase();
    let tool_description = tool.description.to_lowercase();
    let mut score = 0;

    if tool_name == query_lower {
        score += 200;
    } else if tool_name.contains(query_lower) {
        score += 120;
    }

    if tool_description.contains(query_lower) {
        score += 40;
    }

    for query_term in query_terms {
        if tool_name.contains(query_term) {
            score += 25;
        }
        if tool_description.contains(query_term) {
            score += 10;
        }
    }

    score
}

/// Strip noise from inputSchema: remove title, examples, $schema, additionalProperties.
/// Keeps type, properties, required, description (on root only), items, enum.
fn strip_schema(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            let mut clean = serde_json::Map::new();
            for (k, v) in map {
                match k.as_str() {
                    "title"
                    | "examples"
                    | "$schema"
                    | "additionalProperties"
                    | "$id"
                    | "$comment"
                    | "default" => continue,
                    "properties" => {
                        if let Some(props) = v.as_object() {
                            let mut cleaned_props = serde_json::Map::new();
                            for (pk, pv) in props {
                                cleaned_props.insert(pk.clone(), strip_schema(pv));
                            }
                            clean.insert(k.clone(), serde_json::Value::Object(cleaned_props));
                        }
                    }
                    "items" => {
                        clean.insert(k.clone(), strip_schema(v));
                    }
                    _ => {
                        clean.insert(k.clone(), v.clone());
                    }
                }
            }
            serde_json::Value::Object(clean)
        }
        other => other.clone(),
    }
}

fn spawn_catalog_refresh(
    reason: String,
    manager: Arc<ChildManager>,
    engine: Arc<Mutex<SearchEngine>>,
    names_to_refresh: Vec<String>,
    removed_names: Vec<String>,
    server_hashes: HashMap<String, String>,
    delay_ms: u64,
    in_progress: Arc<AtomicBool>,
) {
    if names_to_refresh.is_empty() && removed_names.is_empty() {
        return;
    }

    if in_progress
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        eprintln!(
            "[McpHub][INFO] Catalog refresh already running, skipping duplicate request: {}",
            reason
        );
        return;
    }

    tokio::spawn(async move {
        eprintln!(
            "[McpHub][INFO] Rebuilding catalog in background: {}",
            reason
        );
        rebuild_catalog(
            manager,
            engine,
            names_to_refresh,
            removed_names,
            server_hashes,
            delay_ms,
        )
        .await;
        in_progress.store(false, Ordering::SeqCst);
    });
}

/// Rebuild the in-memory search index and persist the refreshed cache.
async fn rebuild_catalog(
    manager: Arc<ChildManager>,
    engine: Arc<Mutex<SearchEngine>>,
    names_to_refresh: Vec<String>,
    removed_names: Vec<String>,
    server_hashes: HashMap<String, String>,
    delay_ms: u64,
) {
    let total = names_to_refresh.len();
    eprintln!(
        "[McpHub][INFO] Preloading {} servers ({}ms stagger)...",
        total, delay_ms
    );

    let mut cache = crate::cache::load_cache().unwrap_or(crate::cache::SchemaCache {
        version: env!("CARGO_PKG_VERSION").to_string(),
        servers: HashMap::new(),
        errors: HashMap::new(),
        server_hashes: HashMap::new(),
    });
    cache.version = env!("CARGO_PKG_VERSION").to_string();

    for removed_name in removed_names {
        cache.servers.remove(&removed_name);
        cache.errors.remove(&removed_name);
        cache.server_hashes.remove(&removed_name);
    }

    for (i, name) in names_to_refresh.iter().enumerate() {
        match manager.start_server(name).await {
            Ok(tools) => {
                cache.servers.insert(name.clone(), tools);
                cache.errors.remove(name);
            }
            Err(e) => {
                eprintln!("[McpHub][ERROR] Failed to start '{}': {}", name, e);
                cache.errors.insert(name.clone(), e);
                cache.servers.remove(name);
            }
        }
        if let Some(server_hash) = server_hashes.get(name) {
            cache
                .server_hashes
                .insert(name.clone(), server_hash.clone());
        }

        // Stagger starts (skip delay after last)
        if i < total - 1 && delay_ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }
    }

    // Build search index
    let all_tools = indexed_tools_from_servers(&cache.servers);
    let mut eng = engine.lock().await;
    eng.build_index(all_tools);
    let tool_count = eng.tool_count();
    drop(eng);

    crate::cache::save_cache_state(&cache);
    eprintln!(
        "[McpHub][INFO] Catalog rebuild complete: {} tools indexed, {} errors",
        tool_count,
        cache.errors.len()
    );
}

/// Applique une config modifiée : serveurs, filtres, index, et rafraîchit le catalogue des
/// serveurs nouveaux ou changés.
async fn apply_config_change(
    engine: Arc<Mutex<SearchEngine>>,
    config_store: Arc<Mutex<ProxyConfig>>,
    child_manager: Arc<ChildManager>,
    catalog_refresh_in_progress: Arc<AtomicBool>,
) {
    let new_config = match crate::config::try_auto_detect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[McpHub][ERROR] config invalide, la derniere config valide reste active : {}",
                e
            );
            return;
        }
    };
    let new_servers = new_config.servers.clone();
    let new_server_hashes = crate::cache::server_config_hashes(&new_servers);
    let existing_cache = crate::cache::load_cache();
    let mut refresh_server_names: Vec<String> =
        new_servers.keys().cloned().collect();
    refresh_server_names.sort();
    let mut removed_server_names = Vec::new();

    if let Some(cached) = existing_cache {
        refresh_server_names.retain(|server_name| {
            cached.server_hashes.get(server_name)
                != new_server_hashes.get(server_name)
                || cached.errors.contains_key(server_name)
                || !cached.servers.contains_key(server_name)
        });

        removed_server_names = cached
            .server_hashes
            .keys()
            .filter(|server_name| !new_servers.contains_key(*server_name))
            .cloned()
            .collect();
    }

    crate::toolguard::configure(new_config.guard.clone());
    {
        let mut cfg = config_store.lock().await;
        *cfg = new_config;
    }
    // Filtres et descriptions peuvent avoir changé sans redémarrage de serveur.
    if let Some(cached) = crate::cache::load_cache() {
        let all_tools = indexed_tools_from_servers(&cached.servers);
        engine.lock().await.build_index(all_tools);
    }

    child_manager.update_configs(new_servers).await;
    eprintln!("[McpHub][INFO] Config hot-reloaded");

    let delay_ms = {
        let cfg = config_store.lock().await;
        cfg.preload_delay_ms
    };

    spawn_catalog_refresh(
        "config changed".to_string(),
        child_manager.clone(),
        engine.clone(),
        refresh_server_names,
        removed_server_names,
        new_server_hashes,
        delay_ms,
        catalog_refresh_in_progress.clone(),
    );
}

/// Watches schema-cache.json and config.json for changes and hot-reloads them.
async fn config_and_cache_watcher(
    engine: Arc<Mutex<SearchEngine>>,
    config_store: Arc<Mutex<ProxyConfig>>,
    child_manager: Arc<ChildManager>,
    catalog_refresh_in_progress: Arc<AtomicBool>,
) {
    use std::time::SystemTime;

    let cache_path_opt = crate::cache::cache_path();
    let mut last_cache_modified: Option<SystemTime> = cache_path_opt
        .as_ref()
        .and_then(|p| p.metadata().ok())
        .and_then(|m| m.modified().ok());

    let config_path_opt = Some(crate::instance::hub_dir().join("config.json"));
    let mut last_config_modified: Option<SystemTime> = config_path_opt
        .as_ref()
        .and_then(|p| p.metadata().ok())
        .and_then(|m| m.modified().ok());

    let pins_path = crate::toolguard::pins_path();
    let mut last_pins_modified: Option<SystemTime> =
        pins_path.metadata().ok().and_then(|m| m.modified().ok());

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // Épingles modifiées (ex. `McpHub pins approve`) : un outil sorti de quarantaine
        // doit réapparaître dans le catalogue.
        let pins_modified = pins_path.metadata().ok().and_then(|m| m.modified().ok());
        if pins_modified != last_pins_modified {
            if let Some(cached) = crate::cache::load_cache() {
                let all_tools = indexed_tools_from_servers(&cached.servers);
                let mut eng = engine.lock().await;
                eng.build_index(all_tools);
                eprintln!("[McpHub][GUARD] Catalogue reconstruit (epingles modifiees) : {} outils", eng.tool_count());
            }
            // L'index peut lui-même avoir réécrit le fichier (premier contact) : on suit.
            last_pins_modified = pins_path.metadata().ok().and_then(|m| m.modified().ok());
        }

        // Check Cache
        if let Some(cache_path) = &cache_path_opt {
            if let Ok(m) = cache_path.metadata() {
                if let Ok(current_modified) = m.modified() {
                    if Some(current_modified) != last_cache_modified {
                        last_cache_modified = Some(current_modified);

                        if let Some(cached) = crate::cache::load_cache() {
                            let all_tools = indexed_tools_from_servers(&cached.servers);
                            let mut eng = engine.lock().await;
                            eng.build_index(all_tools);
                            eprintln!(
                                "[McpHub][INFO] Cache hot-reloaded: {} tools",
                                eng.tool_count()
                            );
                        }
                    }
                }
            }
        }

        // Check Config
        if let Some(config_path) = &config_path_opt {
            if let Ok(m) = config_path.metadata() {
                if let Ok(current_modified) = m.modified() {
                    if Some(current_modified) != last_config_modified {
                        last_config_modified = Some(current_modified);

                        apply_config_change(
                            engine.clone(),
                            config_store.clone(),
                            child_manager.clone(),
                            catalog_refresh_in_progress.clone(),
                        )
                        .await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hub_with_permissions(permissions: crate::permissions::PermissionsConfig) -> ProxyServer {
        ProxyServer::new(ProxyConfig {
            permissions,
            audit: crate::audit::AuditConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn installation_par_l_agent_demande_confirmation_par_defaut() {
        let hub = hub_with_permissions(crate::permissions::PermissionsConfig {
            confirm_via_dialog: false,
            ..Default::default()
        });
        let response = hub
            .handle_hub_tool(Some(json!(1)), "registry_install", json!({"name": "io.example/x"}), None)
            .await;
        let result = response.result.expect("demande de confirmation");
        assert_eq!(result["_meta"]["mcphub"]["status"], "confirmation_required");

        let response = hub
            .handle_hub_tool(Some(json!(2)), "registry_install", json!({"name": "io.example/x"}), Some("faux"))
            .await;
        assert!(response.error.is_some(), "un jeton inconnu est refuse");
    }

    #[tokio::test]
    async fn regle_explicite_sur_l_installation_respectee() {
        let hub = hub_with_permissions(
            crate::permissions::PermissionsConfig::from_json(&json!({
                "confirmVia": "token",
                "rules": [{ "server": "mcphub", "tool": "registry_install", "action": "deny" }]
            }))
            .unwrap(),
        );
        let response = hub
            .handle_hub_tool(Some(json!(1)), "registry_install", json!({"name": "io.example/x"}), None)
            .await;
        assert!(response.error.expect("refus").message.contains("interdit"));
    }

    #[test]
    fn test_live_discover_results_prioritizes_exact_tool_name() {
        let tools = vec![
            ToolDef {
                name: "deploy".to_string(),
                description: "Deploy a worker".to_string(),
                input_schema: json!({}),
                ..Default::default()
            },
            ToolDef {
                name: "deploy_worker".to_string(),
                description: "Deploy a worker with more options".to_string(),
                input_schema: json!({}),
                ..Default::default()
            },
        ];

        let results = live_discover_results("cloudflare", &tools, "deploy", 2);
        assert_eq!(results[0]["tool"], json!("deploy"));
    }

    #[test]
    fn test_live_discover_results_returns_all_tools_for_empty_query() {
        let tools = vec![ToolDef {
            name: "create_issue".to_string(),
            description: "Create a GitHub issue".to_string(),
            input_schema: json!({}),
            ..Default::default()
        }];

        let results = live_discover_results("github", &tools, "", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["server"], json!("github"));
    }

    #[test]
    fn test_infer_discover_server_filter_prefers_explicit_server() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![IndexedTool {
            name: "supabase__execute_sql".to_string(),
            original_name: "execute_sql".to_string(),
            server_name: "supabase".to_string(),
            description: "Execute SQL".to_string(),
            tool_def: ToolDef {
                name: "execute_sql".to_string(),
                description: "Execute SQL".to_string(),
                input_schema: json!({}),
                ..Default::default()
            },
            untrusted_output: false,
        }]);

        let server_names = vec!["supabase".to_string(), "github".to_string()];
        let (server_filter, filter_source) =
            infer_discover_server_filter("execute sql", Some("github"), &server_names, &engine);

        assert_eq!(server_filter, Some("github".to_string()));
        assert_eq!(filter_source, Some(DiscoverFilterSource::ExplicitServer));
    }

    #[test]
    fn test_discover_routing_reason_mentions_tool_name_inference() {
        let reason = discover_routing_reason(
            DiscoverRouteStrategy::IndexedBm25,
            Some(DiscoverFilterSource::QueryToolName),
            Some("supabase"),
        );

        assert!(reason.contains("tool-name"));
        assert!(reason.contains("supabase"));
    }

    #[test]
    fn test_infer_discover_server_filter_prefers_specific_tool_match_before_server_name() {
        let mut engine = SearchEngine::new();
        engine.build_index(vec![IndexedTool {
            name: "cloudflare-docs__search_cloudflare_documentation".to_string(),
            original_name: "search_cloudflare_documentation".to_string(),
            server_name: "cloudflare-docs".to_string(),
            description: "Search Cloudflare documentation".to_string(),
            tool_def: ToolDef {
                name: "search_cloudflare_documentation".to_string(),
                description: "Search Cloudflare documentation".to_string(),
                input_schema: json!({}),
                ..Default::default()
            },
            untrusted_output: false,
        }]);

        let server_names = vec![
            "cloudflare".to_string(),
            "cloudflare-docs".to_string(),
            "cloudflare-agents-docs".to_string(),
        ];
        let (server_filter, filter_source) = infer_discover_server_filter(
            "search cloudflare documentation",
            None,
            &server_names,
            &engine,
        );

        assert_eq!(server_filter, Some("cloudflare-docs".to_string()));
        assert_eq!(filter_source, Some(DiscoverFilterSource::QueryToolName));
    }
}
