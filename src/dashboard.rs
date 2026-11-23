//! Embedded web dashboard for McpHub.
//! Serves HTML + JSON API on http://127.0.0.1:24680
//! Zero external dependencies — uses tokio::net::TcpListener directly.

use crate::protocol::JsonRpcRequest;
use crate::proxy::ProxyServer;
use crate::sse::{extract_session_id, SseManager};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─── Config I/O ──────────────────────────────────────────────

fn config_dir() -> PathBuf {
    crate::instance::hub_dir()
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

fn cache_path() -> PathBuf {
    config_dir().join("schema-cache.json")
}

pub fn get_auth_token() -> String {
    let path = config_dir().join("auth-token");
    if let Ok(token) = fs::read_to_string(&path) {
        let token = token.trim().to_string();
        // Les anciens jetons (horodatage en hexadécimal) sont devinables : on les remplace.
        if crate::http_guard::token_is_current(&token) {
            return token;
        }
    }
    let Some(token) = crate::http_guard::random_token() else {
        // Sans source aléatoire, on garde l'existant plutôt que d'en fabriquer un faible.
        return fs::read_to_string(&path)
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Écriture atomique, directement en mode 600 (jamais lisible par les autres, même un instant).
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        eprintln!("[McpHub][SECURITY] Nouveau jeton d'authentification genere dans {}", path.display());
    }
    token
}

fn binary_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_default()
            .join("McpHub")
            .join("target")
            .join("release")
            .join("McpHub")
    })
}

/// L'erreur est déjà la réponse HTTP : une config illisible bloque toute modification.
fn read_config() -> Result<Value, Vec<u8>> {
    match crate::config::read_raw(&config_path()) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Ok(json!({"mcpServers": {}, "settings": {"mode": "discover", "idleTimeout": 300}})),
        Err(e) => Err(json_err(409, &e)),
    }
}

fn save_config(config: &Value) -> Result<(), Vec<u8>> {
    crate::config::write_raw_atomic(&config_path(), config)
        .map_err(|e| json_err(500, &format!("Failed to save config: {}", e)))
}

/// What the dashboard needs from schema-cache.json, per server: tool names and
/// an estimate of the schema size in tokens, plus the cached start errors.
struct CacheSummary {
    servers: HashMap<String, CachedServer>,
    errors: HashMap<String, String>,
}

struct CachedServer {
    tool_names: Vec<String>,
    schema_tokens: usize,
}

fn summarize_cache(cache: &Value) -> CacheSummary {
    let servers = cache
        .get("servers")
        .and_then(|v| v.as_object())
        .map(|servers| {
            servers
                .iter()
                .map(|(name, tools)| {
                    let tool_names = tools
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let schema_tokens = tools.to_string().len() / 4;
                    (name.clone(), CachedServer { tool_names, schema_tokens })
                })
                .collect()
        })
        .unwrap_or_default();
    let errors = cache
        .get("errors")
        .and_then(|v| v.as_object())
        .map(|errors| {
            errors
                .iter()
                .filter_map(|(name, error)| error.as_str().map(|e| (name.clone(), e.to_string())))
                .collect()
        })
        .unwrap_or_default();
    CacheSummary { servers, errors }
}

/// Modification time and size of the cache file: the summary is rebuilt only
/// when one of them changes (the hub rewrites the whole file on every update).
type CacheStamp = (SystemTime, u64);

static CACHE_SUMMARY: Mutex<Option<(CacheStamp, Arc<CacheSummary>)>> = Mutex::new(None);

/// Parsing the 10+ MB schema cache on every /api/servers request cost about
/// 250 ms; the summary is kept in memory and reused while the file is unchanged.
fn read_cache_summary() -> Option<Arc<CacheSummary>> {
    let path = cache_path();
    let metadata = fs::metadata(&path).ok()?;
    let stamp = metadata.modified().ok().map(|modified| (modified, metadata.len()));
    let mut slot = CACHE_SUMMARY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let (Some(stamp), Some((cached_stamp, summary))) = (stamp, slot.as_ref()) {
        if stamp == *cached_stamp {
            return Some(Arc::clone(summary));
        }
    }
    let parsed: Option<Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(cache) = parsed else {
        *slot = None;
        return None;
    };
    let summary = Arc::new(summarize_cache(&cache));
    *slot = stamp.map(|stamp| (stamp, Arc::clone(&summary)));
    Some(summary)
}

// ─── HTTP Parsing ────────────────────────────────────────────

struct HttpRequest {
    method: String,
    path: String,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

fn parse_request(raw: &str) -> Option<HttpRequest> {
    let mut lines = raw.lines();
    let first = lines.next()?;
    let parts: Vec<&str> = first.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut headers = std::collections::HashMap::new();
    let mut content_length: usize = 0;
    for line in raw.lines().skip(1) {
        if line.is_empty() || line == "\r" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let body = if content_length > 0 {
        if let Some(idx) = raw.find("\r\n\r\n") {
            raw[idx + 4..].to_string()
        } else if let Some(idx) = raw.find("\n\n") {
            raw[idx + 2..].to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn http_response(status: u16, status_text: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{}",
        status, status_text, content_type, body.len(), body
    )
    .into_bytes()
}

fn http_response_with(
    status: u16,
    status_text: &str,
    content_type: &str,
    body: &str,
    extra_headers: &str,
) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\n{}Connection: close\r\n\r\n{}",
        status, status_text, content_type, body.len(), extra_headers, body
    )
    .into_bytes()
}

fn json_ok(data: Value) -> Vec<u8> {
    http_response(200, "OK", "application/json", &data.to_string())
}

fn json_err(status: u16, msg: &str) -> Vec<u8> {
    http_response(
        status,
        "Error",
        "application/json",
        &json!({"error": msg}).to_string(),
    )
}

/// Streamable HTTP sans état (spec MCP 2025-06-18 et 2026-07-28) : un POST = un message
/// JSON-RPC, réponse JSON directe ; une notification seule reçoit 202 sans corps.
/// Les lots (tableau JSON, ancienne spec 2025-03-26) restent acceptés.
async fn handle_streamable_http_message(
    body: &str,
    proxy: &Arc<ProxyServer>,
    profile: Option<String>,
) -> Vec<u8> {
    let rpc_error = |code: i64, msg: &str| {
        http_response(
            400,
            "Bad Request",
            "application/json",
            &json!({"jsonrpc":"2.0","id":null,"error":{"code":code,"message":msg}}).to_string(),
        )
    };
    let parsed: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return rpc_error(-32700, &format!("Parse error: {}", e)),
    };
    let (items, is_batch) = match parsed {
        Value::Array(a) if !a.is_empty() => (a, true),
        Value::Array(_) => return rpc_error(-32600, "Empty batch"),
        other => (vec![other], false),
    };
    // Validation complète AVANT d'exécuter quoi que ce soit.
    let mut requests = Vec::new();
    for item in items {
        let is_client_response = item.get("method").is_none()
            && (item.get("result").is_some() || item.get("error").is_some());
        if is_client_response {
            continue; // réponse du client (ex. à une sollicitation) : rien à renvoyer
        }
        match serde_json::from_value::<JsonRpcRequest>(item) {
            Ok(r) => requests.push(r),
            Err(e) => return rpc_error(-32600, &format!("Invalid Request: {}", e)),
        }
    }
    let mut responses = Vec::new();
    for request in requests {
        if let Some(resp) = proxy.handle_request_in_profile(request, profile.clone()).await {
            responses.push(resp);
        }
    }
    if responses.is_empty() {
        return http_response_with(202, "Accepted", "text/plain", "", "");
    }
    let out = if is_batch {
        serde_json::to_string(&responses)
    } else {
        serde_json::to_string(&responses[0])
    };
    match out {
        Ok(body) => http_response(200, "OK", "application/json", &body),
        Err(e) => json_err(500, &format!("Serialize failed: {}", e)),
    }
}

#[derive(Clone, Copy)]
enum OauthPage {
    Connected,
    Denied,
    Expired,
}

/// Premier code de langue de l'en-tête Accept-Language (ex. « fr-FR,fr;q=0.9,en;q=0.8 » donne le français).
fn prefers_french(accept_language: &str) -> bool {
    accept_language
        .split(',')
        .next()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .is_some_and(|tag| tag == "fr" || tag.starts_with("fr-") || tag.starts_with("fr;"))
}

fn oauth_callback_page(page: OauthPage, french: bool) -> String {
    let (tone, icon) = match page {
        OauthPage::Connected => ("ok", r#"<path pathLength="1" d="M5 12.5l4.5 4.5L19 7.5"/>"#),
        OauthPage::Denied => ("denied", r#"<path pathLength="1" d="M7 7l10 10"/><path pathLength="1" d="M17 7L7 17"/>"#),
        OauthPage::Expired => (
            "expired",
            r#"<path pathLength="1" d="M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z"/><path pathLength="1" d="M12 7v5l3 2"/>"#,
        ),
    };
    let (title, message) = match (page, french) {
        (OauthPage::Connected, true) => ("Connexion réussie", "Le serveur est maintenant relié à McpHub. Vous pouvez fermer cet onglet."),
        (OauthPage::Connected, false) => ("You're connected", "The server is now linked to McpHub. You can close this tab."),
        (OauthPage::Denied, true) => ("Connexion refusée", "L'autorisation n'a pas été accordée. Fermez cet onglet et relancez la connexion."),
        (OauthPage::Denied, false) => ("Connection declined", "Access was not granted. Close this tab and start the sign-in again."),
        (OauthPage::Expired, true) => ("Lien expiré", "Ce lien de connexion est inconnu ou a expiré. Relancez <code>McpHub oauth login SERVEUR</code>."),
        (OauthPage::Expired, false) => ("Link expired", "This sign-in link is unknown or has expired. Run <code>McpHub oauth login SERVER</code> again."),
    };
    OAUTH_CALLBACK_HTML
        .replace("{{lang}}", if french { "fr" } else { "en" })
        .replace("{{tone}}", tone)
        .replace("{{icon}}", icon)
        .replace("{{title}}", title)
        .replace("{{message}}", message)
}

fn query_param(full_path: &str, key: &str) -> Option<String> {
    let query = full_path.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            Some(urldecode(v))
        } else {
            None
        }
    })
}

/// Profil demandé par le client : /mcp/<profil> ou ?profile=<profil>. Profil inconnu = 404.
fn requested_profile(full_path: &str) -> Result<Option<String>, Vec<u8>> {
    let path = full_path.split('?').next().unwrap_or(full_path);
    let from_path = path
        .strip_prefix("/mcp/")
        .map(|p| urldecode(p.trim_matches('/')))
        .filter(|p| !p.is_empty());
    let name = from_path.or_else(|| query_param(full_path, "profile").filter(|p| !p.is_empty()));
    match name {
        Some(n) if !crate::toolguard::profile_exists(&n) => {
            Err(json_err(404, &format!("Profil McpHub inconnu : {}", n)))
        }
        other => Ok(other),
    }
}

// ─── API Handlers ────────────────────────────────────────────

fn handle_get_servers() -> Vec<u8> {
    let config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let cache = read_cache_summary();
    let servers_obj = config
        .get("mcpServers")
        .or_else(|| config.get("servers"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut result: Vec<Value> = Vec::new();
    let mut names: Vec<String> = servers_obj.keys().cloned().collect();
    names.sort();
    // Noms seulement : connexions OAuth et jetons présents dans le coffre de McpHub.
    let stored: std::collections::BTreeSet<String> = crate::vault::list_names().into_iter().collect();

    for name in &names {
        let srv = &servers_obj[name];
        let cached = cache.as_ref().and_then(|c| c.servers.get(name));
        let tools: Vec<String> = cached.map(|c| c.tool_names.clone()).unwrap_or_default();
        let tool_count = tools.len();

        let error_msg = cache
            .as_ref()
            .and_then(|c| c.errors.get(name))
            .map(String::as_str)
            .unwrap_or("");

        result.push(json!({
            "name": name,
            "command": srv.get("command").and_then(|v| v.as_str()).unwrap_or(""),
            "args": srv.get("args").unwrap_or(&json!([])),
            "env": srv.get("env").unwrap_or(&json!({})),
            "disabled": srv.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            "tools": tool_count,
            "schemaTokens": cached.map(|c| c.schema_tokens).unwrap_or(0),
            "url": srv.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            "toolNames": tools,
            "status": if cached.is_some() { "cached" } else if !error_msg.is_empty() { "error" } else { "uncached" },
            "error": error_msg,
            "oauth": {
                "capable": is_oauth_server(srv),
                "connected": stored.contains(&crate::oauth::vault_name(name)),
            },
            "keys": crate::tenant::referenced_vars(srv)
                .into_iter()
                .map(|var| {
                    json!({
                        "present": stored.contains(&var) || std::env::var(&var).is_ok(),
                        "connector": crate::connectors::describe(&var, None),
                        "var": var,
                    })
                })
                .collect::<Vec<_>>(),
        }));
    }

    let total_tools: usize = result
        .iter()
        .map(|s| s["tools"].as_u64().unwrap_or(0) as usize)
        .sum();

    json_ok(json!({
        "servers": result,
        "settings": config.get("settings").unwrap_or(&json!({})),
        "totalTools": total_tools,
        "cacheExists": cache.is_some(),
        "securityAlerts": crate::toolguard::security_alerts()
    }))
}

fn handle_add_server(body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let name = match data.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return json_err(400, "Name required"),
    };
    let command = match data.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return json_err(400, "Command required"),
    };

    let args = if let Some(s) = data.get("args").and_then(|v| v.as_str()) {
        Value::Array(
            s.split_whitespace()
                .map(|a| Value::String(a.to_string()))
                .collect(),
        )
    } else {
        data.get("args").cloned().unwrap_or(json!([]))
    };

    let env = data.get("env").cloned().unwrap_or(json!({}));

    let mut config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let key = if config.get("servers").is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    if config.get(key).is_none() {
        config[key] = json!({});
    }
    config[key][&name] = json!({
        "command": command,
        "args": args,
        "env": env
    });

    match save_config(&config) {
        Ok(()) => json_ok(json!({"ok": true, "message": "Server added"})),
        Err(resp) => resp,
    }
}

fn handle_update_server(name: &str, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };

    let mut config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    let servers = match config.get_mut(key).and_then(|v| v.as_object_mut()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };

    if !servers.contains_key(name) {
        return json_err(404, "Server not found");
    }

    let new_name = data.get("newName").and_then(|v| v.as_str()).unwrap_or(name);

    if new_name != name {
        if servers.contains_key(new_name) {
            return json_err(409, "A server with this name already exists");
        }
        let existing = servers.remove(name).unwrap();
        servers.insert(new_name.to_string(), existing);
    }

    let srv = servers.get_mut(new_name).unwrap();
    if let Some(cmd) = data.get("command").and_then(|v| v.as_str()) {
        srv["command"] = json!(cmd);
    }
    if let Some(args) = data.get("args") {
        if let Some(s) = args.as_str() {
            srv["args"] = Value::Array(
                s.split_whitespace()
                    .map(|a| Value::String(a.to_string()))
                    .collect(),
            );
        } else {
            srv["args"] = args.clone();
        }
    }
    if let Some(env) = data.get("env") {
        srv["env"] = env.clone();
    }

    match save_config(&config) {
        Ok(()) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

fn handle_delete_server(name: &str) -> Vec<u8> {
    let mut config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    let servers = match config.get_mut(key).and_then(|v| v.as_object_mut()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };
    if servers.remove(name).is_none() {
        return json_err(404, "Server not found");
    }
    match save_config(&config) {
        Ok(()) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

fn handle_toggle_server(name: &str, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let disabled = data
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    let servers = match config.get_mut(key).and_then(|v| v.as_object_mut()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };
    let server = match servers.get_mut(name) {
        Some(s) => s,
        None => return json_err(404, "Server not found"),
    };
    if let Some(obj) = server.as_object_mut() {
        if disabled {
            obj.insert("disabled".to_string(), json!(true));
        } else {
            obj.remove("disabled");
        }
    }
    match save_config(&config) {
        Ok(()) => json_ok(json!({"ok": true, "disabled": disabled})),
        Err(resp) => resp,
    }
}

fn handle_get_settings() -> Vec<u8> {
    let config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let settings = config
        .get("settings")
        .cloned()
        .unwrap_or(json!({"mode": "discover", "idleTimeout": 300}));
    json_ok(settings)
}

/// Tenant déclaré et serveur qui lui appartient.
fn tenant_server(server: &str, tenant: &str) -> Result<crate::tenant::TenantDef, String> {
    let tenants = crate::tenant::load_from(&crate::tenant::projects_path())?;
    let def = tenants.get(tenant).ok_or_else(|| format!("tenant {} inconnu", tenant))?;
    if !def.owns(server) {
        return Err(format!("{} n'est pas un serveur du tenant {}", server, tenant));
    }
    Ok(def.clone())
}

/// Connexion OAuth d'un serveur pour un tenant : la page d'autorisation n'est PAS ouverte dans
/// le navigateur par défaut (connecté aux comptes McpHub) ; son adresse est renvoyée pour être
/// ouverte dans le profil de navigateur du client. Jetons enregistrés dans le coffre du tenant.
async fn tenant_oauth_login(server: &str, tenant: &str) -> Vec<u8> {
    let def = match tenant_server(server, tenant) {
        Ok(d) => d,
        Err(e) => return json_err(400, &e),
    };
    let base = server.to_string();
    let derived = tokio::task::spawn_blocking(move || crate::tenant::derive_from_vault(&def, &base)).await;
    let config = match derived {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return json_err(400, &e),
        Err(e) => return json_err(500, &e.to_string()),
    };
    let Some(url) = config.url.filter(|_| config.oauth) else {
        return json_err(400, "serveur sans OAuth gere par McpHub (pas d'url ou auth != oauth)");
    };
    let instance = crate::tenant::instance_name(server, tenant);
    match crate::oauth::prepare_login(&instance, &url, None).await {
        Ok(prepared) => {
            let auth_url = prepared.auth_url.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::oauth::finish_login(prepared).await {
                    eprintln!("[McpHub][OAUTH] {} : echec de connexion : {}", instance, e);
                }
            });
            json_ok(json!({ "ok": true, "url": auth_url }))
        }
        Err(e) => json_err(400, &e),
    }
}

async fn handle_get_metrics(
    proxy: Option<Arc<ProxyServer>>,
    sse: Option<Arc<SseManager>>,
) -> Vec<u8> {
    if let Some(p) = proxy {
        let mut m = p.metrics.lock().await;
        if let Some(s) = sse {
            m.active_sse_sessions = s.session_count().await;
        }
        json_ok(json!(*m))
    } else {
        json_err(503, "Metrics not available in dashboard-only mode")
    }
}

fn handle_update_settings(body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let mut config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if let Some(existing) = config.get_mut("settings").and_then(|v| v.as_object_mut()) {
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                existing.insert(k.clone(), v.clone());
            }
        }
    } else {
        config["settings"] = data;
    }
    match save_config(&config) {
        Ok(()) => json_ok(json!({"ok": true, "settings": config["settings"]})),
        Err(resp) => resp,
    }
}

async fn handle_generate() -> Vec<u8> {
    let bin = binary_path();
    if !bin.exists() {
        return json_err(500, "Binary not found");
    }
    let output = tokio::process::Command::new(&bin)
        .arg("generate")
        .output()
        .await;

    match output {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let combined = format!("{}{}", stderr, stdout);

            let mut server_results: Vec<Value> = Vec::new();
            for line in combined.lines() {
                if let Some(caps) = line.find("] ").and_then(|i| {
                    let rest = &line[i + 2..];
                    let name_end = rest.find(" ...")?;
                    let name = &rest[..name_end];
                    if rest.contains("FAILED") {
                        Some((name.to_string(), 0, false))
                    } else {
                        let tools_str = rest
                            .find("... ")
                            .map(|j| &rest[j + 4..])
                            .and_then(|s| s.split_whitespace().next())
                            .and_then(|n| n.parse::<usize>().ok())
                            .unwrap_or(0);
                        Some((name.to_string(), tools_str, true))
                    }
                }) {
                    server_results.push(json!({
                        "name": caps.0,
                        "tools": caps.1,
                        "ok": caps.2
                    }));
                }
            }

            let summary = if let Some(idx) = combined.find("Done:") {
                let rest = &combined[idx..];
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let ok_count = parts
                    .get(1)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let failed = parts
                    .get(3)
                    .and_then(|s| s.trim_end_matches(',').parse::<usize>().ok())
                    .unwrap_or(0);
                let total = parts
                    .get(5)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                Some(json!({"ok": ok_count, "failed": failed, "totalTools": total}))
            } else {
                None
            };

            json_ok(json!({
                "ok": out.status.success(),
                "servers": server_results,
                "summary": summary
            }))
        }
        Err(e) => json_err(500, &format!("Failed to run generate: {}", e)),
    }
}

// ─── Repair Handler ─────────────────────────────────────────

async fn handle_repair_server(name: &str) -> Vec<u8> {
    let config = match read_config() {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    let servers = match config.get(key).and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };
    if !servers.contains_key(name) {
        return json_err(404, "Server not found");
    }

    // Même config que le démon (${VAR} résolus, sandbox), pas le JSON brut.
    let server_config = match crate::config::try_auto_detect() {
        Ok(cfg) => match cfg.servers.get(name) {
            Some(s) if s.url.is_some() => {
                return json_ok(json!({
                    "ok": false,
                    "step": "remote_server",
                    "error": "Remote server: there is no local command to test",
                    "suggestion": "Check its URL and headers, or sign in with OAuth",
                    "auto_fixable": false
                }))
            }
            Some(s) => s.clone(),
            None => {
                return json_ok(json!({
                    "ok": false,
                    "step": "not_loaded",
                    "error": "Server is disabled or skipped by McpHub",
                    "suggestion": "Enable it, then run the diagnosis again",
                    "auto_fixable": false
                }))
            }
        },
        Err(e) => return json_err(409, &e),
    };
    let command = server_config.command.as_str();

    // Step 1: Check if command exists
    let cmd_check = tokio::process::Command::new("which")
        .arg(command)
        .output()
        .await;

    let cmd_exists = cmd_check.map(|o| o.status.success()).unwrap_or(false);
    if !cmd_exists {
        // Try to find the command in common locations
        let common_paths = [
            format!(
                "{}/.nvm/versions/node/v25.0.0/bin/{}",
                dirs::home_dir().unwrap_or_default().display(),
                command
            ),
            format!(
                "{}/.nvm/versions/node/v22.22.0/bin/{}",
                dirs::home_dir().unwrap_or_default().display(),
                command
            ),
            format!("/opt/homebrew/bin/{}", command),
            format!("/usr/local/bin/{}", command),
        ];
        let mut found_path = None;
        for p in &common_paths {
            if std::path::Path::new(p).exists() {
                found_path = Some(p.clone());
                break;
            }
        }

        if let Some(path) = found_path {
            return json_ok(json!({
                "ok": false,
                "step": "command_not_in_path",
                "error": format!("Command '{}' not in PATH but found at: {}", command, path),
                "suggestion": format!("Update server command to: {}", path),
                "auto_fixable": true,
                "fix_command": path
            }));
        }

        return json_ok(json!({
            "ok": false,
            "step": "command_not_found",
            "error": format!("Command '{}' not found anywhere", command),
            "suggestion": "Check that the binary/package is installed",
            "auto_fixable": false
        }));
    }

    // Step 2: start it exactly as the daemon does (sandbox, minimal environment without the
    // daemon's secrets, own process group).
    let mut cmd = crate::child::local_command(name, &server_config);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json_ok(json!({
                "ok": false,
                "step": "spawn_failed",
                "error": format!("Failed to start: {}", e),
                "suggestion": "Check command and arguments",
                "auto_fixable": false
            }));
        }
    };

    // Give it 10 seconds to start
    let pid = child.id();
    let output =
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output()).await;
    if output.is_err() {
        if let Some(pid) = pid {
            crate::child::kill_group(pid);
        }
    }

    match output {
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();

            // Check for common errors
            let combined = format!("{}\n{}", stderr, stdout);

            if combined.contains("MODULE_NOT_FOUND") || combined.contains("Cannot find module") {
                let module = combined
                    .lines()
                    .find(|l| l.contains("Cannot find module"))
                    .unwrap_or("unknown module")
                    .to_string();
                return json_ok(json!({
                    "ok": false,
                    "step": "module_not_found",
                    "error": module,
                    "suggestion": "Run: npm install or rebuild the project",
                    "auto_fixable": false
                }));
            }

            if combined.contains("ENOENT") {
                return json_ok(json!({
                    "ok": false,
                    "step": "file_not_found",
                    "error": "A file referenced by the server does not exist",
                    "detail": combined.lines().find(|l| l.contains("ENOENT")).unwrap_or("").to_string(),
                    "suggestion": "Check file paths in args",
                    "auto_fixable": false
                }));
            }

            if combined.contains("API")
                && (combined.contains("401")
                    || combined.contains("403")
                    || combined.contains("unauthorized")
                    || combined.contains("Unauthorized"))
            {
                return json_ok(json!({
                    "ok": false,
                    "step": "auth_error",
                    "error": "Authentication failed - API key/token may be invalid or expired",
                    "suggestion": "Update the API key/token in env vars",
                    "auto_fixable": false
                }));
            }

            if combined.contains("ECONNREFUSED")
                || combined.contains("fetch failed")
                || combined.contains("network")
            {
                return json_ok(json!({
                    "ok": false,
                    "step": "network_error",
                    "error": "Network connection failed",
                    "suggestion": "Check internet connection or service URL",
                    "auto_fixable": false
                }));
            }

            // If process exited quickly without MCP handshake, it crashed
            if !out.status.success() {
                return json_ok(json!({
                    "ok": false,
                    "step": "crash",
                    "error": format!("Process exited with code {}", out.status.code().unwrap_or(-1)),
                    "detail": combined.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"),
                    "suggestion": "Check server logs above for details",
                    "auto_fixable": false
                }));
            }

            // If we got here, rebuild cache for this server
            let bin = binary_path();
            let gen_output = tokio::process::Command::new(&bin)
                .arg("generate")
                .output()
                .await;

            match gen_output {
                Ok(gen_out) => {
                    let gen_combined = format!(
                        "{}{}",
                        String::from_utf8_lossy(&gen_out.stderr),
                        String::from_utf8_lossy(&gen_out.stdout)
                    );
                    let server_line = gen_combined
                        .lines()
                        .find(|l| l.contains(name))
                        .unwrap_or("");

                    if server_line.contains("FAILED") {
                        let error_part = server_line
                            .split("FAILED:")
                            .nth(1)
                            .unwrap_or("Unknown error")
                            .trim();
                        json_ok(json!({
                            "ok": false,
                            "step": "generate_failed",
                            "error": format!("Cache generation failed: {}", error_part),
                            "suggestion": "Server starts but doesn't respond to MCP protocol",
                            "auto_fixable": false
                        }))
                    } else {
                        json_ok(json!({
                            "ok": true,
                            "step": "repaired",
                            "message": format!("Server '{}' is working and cache has been rebuilt", name)
                        }))
                    }
                }
                Err(e) => json_ok(json!({
                    "ok": false,
                    "step": "generate_error",
                    "error": format!("Cache rebuild failed: {}", e),
                    "auto_fixable": false
                })),
            }
        }
        Ok(Err(e)) => json_ok(json!({
            "ok": false,
            "step": "process_error",
            "error": format!("Process error: {}", e),
            "auto_fixable": false
        })),
        Err(_) => {
            // Timeout - server is still running, which is actually good for MCP servers
            // They stay alive waiting for stdio input. Rebuild cache.
            let bin = binary_path();
            let gen_output = tokio::process::Command::new(&bin)
                .arg("generate")
                .output()
                .await;

            match gen_output {
                Ok(gen_out) => {
                    let gen_combined = format!(
                        "{}{}",
                        String::from_utf8_lossy(&gen_out.stderr),
                        String::from_utf8_lossy(&gen_out.stdout)
                    );
                    if gen_combined.contains(&format!("{} ... ", name))
                        && !gen_combined.contains("FAILED")
                    {
                        json_ok(json!({
                            "ok": true,
                            "step": "repaired",
                            "message": format!("Server '{}' repaired and cache rebuilt", name)
                        }))
                    } else {
                        let error_line = gen_combined
                            .lines()
                            .find(|l| l.contains(name) && l.contains("FAILED"))
                            .unwrap_or("Unknown error");
                        json_ok(json!({
                            "ok": false,
                            "step": "generate_failed",
                            "error": error_line.to_string(),
                            "suggestion": "Server starts but MCP handshake fails",
                            "auto_fixable": false
                        }))
                    }
                }
                Err(e) => json_ok(json!({
                    "ok": false,
                    "step": "generate_error",
                    "error": format!("Cache rebuild failed: {}", e),
                    "auto_fixable": false
                })),
            }
        }
    }
}

// ─── Écran Client accounts (tenants) ─────────────────────────
//
// Aucune route ne renvoie de valeur de secret : l'écran ne voit que des noms et « présent ».
// Les écritures passent par les mêmes fonctions que la CLI (validation, sauvegarde de
// projects.json) ; le retrait d'un client exige la fenêtre macOS native.

/// Taille maximale du corps d'une écriture de secret.
const SECRET_BODY_MAX: usize = 16 * 1024;

/// Serveur distant dont McpHub gère la connexion OAuth (même règle que la config).
fn is_oauth_server(raw: &Value) -> bool {
    if raw.get("url").and_then(|v| v.as_str()).is_none() {
        return false;
    }
    match raw.get("auth").and_then(|v| v.as_str()) {
        Some(a) => a.eq_ignore_ascii_case("oauth"),
        None => !raw
            .get("headers")
            .and_then(|h| h.as_object())
            .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("authorization"))),
    }
}

/// Partie bloquante de l'écran (fichiers et Trousseau) : noms et états, jamais de valeur.
fn tenants_snapshot() -> Result<Value, String> {
    use crate::tenant::{cf_orchestrator_path, cf_orchestrator_projects, instance_name};
    use std::collections::{BTreeMap, BTreeSet};
    let path = crate::tenant::projects_path();
    let version = crate::tenant::projects_version(&path);
    let tenants = crate::tenant::load_from(&path)?;
    let (configured, config_error) = match crate::tenant::configured_servers() {
        Ok(c) => (c, None),
        Err(e) => (BTreeMap::new(), Some(e)),
    };
    let available: Vec<Value> = configured
        .iter()
        .map(|(name, raw)| {
            let (required, optional) = crate::tenant::server_variables(raw);
            json!({ "name": name, "oauth": is_oauth_server(raw), "required": required, "optional": optional })
        })
        .collect();
    let cf_path = cf_orchestrator_path();
    let mut out = Vec::new();
    for t in &tenants.tenants {
        let scope = crate::vault::Scope::Tenant(t.id.clone());
        let stored: BTreeSet<String> = crate::vault::list_names_in(&scope).into_iter().collect();
        let servers: Vec<Value> = t
            .credentials
            .iter()
            .map(|(server, vars)| {
                let instance = instance_name(server, &t.id);
                let raw = configured.get(server);
                let connected = stored.contains(&crate::oauth::vault_name(server));
                let expires_at = if connected {
                    crate::oauth::load(&instance).and_then(|s| s.tokens).and_then(|tk| tk.expires_at)
                } else {
                    None
                };
                json!({
                    "server": server,
                    "instance": instance,
                    "in_config": raw.is_some(),
                    "secrets": vars
                        .iter()
                        .map(|(var, name)| {
                            json!({
                                "var": var,
                                "name": name,
                                "present": stored.contains(name),
                                "connector": crate::connectors::describe(var, Some(&t.id)),
                            })
                        })
                        .collect::<Vec<_>>(),
                    "oauth": {
                        "capable": raw.is_some_and(is_oauth_server),
                        "connected": connected,
                        "expires_at": expires_at,
                    },
                })
            })
            .collect();
        let cf = match &cf_path {
            None => json!({ "projects": [], "error": "dossier personnel introuvable" }),
            Some(p) => match cf_orchestrator_projects(p, &t.id) {
                Ok(projects) => json!({ "projects": projects }),
                Err(e) => json!({ "projects": [], "error": e }),
            },
        };
        out.push(json!({
            "id": t.id,
            "project": t.project,
            "workspaces": t.workspaces,
            "servers": servers,
            "shared_servers": t.shared_servers,
            "cf": cf,
            "browser": tenant_browser_profile(&t.id).as_ref().map(profile_json),
        }));
    }
    Ok(json!({
        "version": version,
        "projects_path": path,
        "tenants": out,
        "available": available,
        "config_error": config_error,
    }))
}

/// Tenants déclarés, état de leurs instances et métriques (noms de secrets seulement).
async fn handle_get_tenants(proxy: Option<Arc<ProxyServer>>) -> Vec<u8> {
    let mut snapshot = match tokio::task::spawn_blocking(tenants_snapshot).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return json_err(500, &e),
        Err(e) => return json_err(500, &e.to_string()),
    };
    if let Some(p) = &proxy {
        let metrics = p.metrics.lock().await.servers.clone();
        let Some(tenants) = snapshot.get_mut("tenants").and_then(|t| t.as_array_mut()) else {
            return json_ok(snapshot);
        };
        for tenant in tenants.iter_mut() {
            let Some(servers) = tenant.get_mut("servers").and_then(|s| s.as_array_mut()) else {
                continue;
            };
            for server in servers.iter_mut() {
                let instance = server.get("instance").and_then(|i| i.as_str()).unwrap_or_default().to_string();
                server["running"] = json!(p.instance_running(&instance).await);
                server["metrics"] = match metrics.get(&instance) {
                    Some(m) => json!({ "calls": m.call_count, "errors": m.error_count, "last_error": m.last_error }),
                    None => Value::Null,
                };
            }
        }
    }
    json_ok(snapshot)
}

/// Action du tableau de bord sur un tenant, notée au journal d'audit (noms seulement).
async fn audit_tenant_action(
    proxy: &Option<Arc<ProxyServer>>,
    tenant: &str,
    action: String,
    result: &Result<(), String>,
) {
    audit_dashboard_action(proxy, Some(tenant), action, result).await
}

/// Action du tableau de bord (compte McpHub ou tenant), notée au journal d'audit.
async fn audit_dashboard_action(
    proxy: &Option<Arc<ProxyServer>>,
    tenant: Option<&str>,
    action: String,
    result: &Result<(), String>,
) {
    let entry = crate::audit::AuditEntry {
        server: "mcphub".into(),
        tool: action,
        mode: "dashboard",
        decision: "allow",
        policy: "dashboard".into(),
        status: if result.is_ok() { "ok" } else { "error" },
        error: result.as_ref().err().cloned(),
        tenant: tenant.map(String::from),
        ..Default::default()
    };
    match proxy {
        Some(p) => p.record_dashboard_audit(entry).await,
        None => {
            let settings = read_config()
                .ok()
                .and_then(|c| c.get("settings").and_then(|s| s.get("audit")).cloned())
                .unwrap_or_else(|| json!({}));
            let cfg = crate::audit::AuditConfig::from_json(&settings);
            crate::audit::AuditLogger::new(crate::audit::default_path()).record(&cfg, entry);
        }
    }
}

/// Erreur d'écriture de projects.json : 409 si le fichier a changé depuis l'affichage.
fn projects_write_error(e: &str) -> Vec<u8> {
    if e == crate::tenant::STALE_PROJECTS {
        json_err(409, e)
    } else {
        json_err(400, e)
    }
}

fn required_str<'a>(data: &'a Value, key: &str) -> Result<&'a str, Vec<u8>> {
    data.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| json_err(400, &format!("{} requis", key)))
}

async fn reset_tenant(proxy: &Option<Arc<ProxyServer>>, id: &str) {
    if let Some(p) = proxy {
        p.reset_tenant(id).await;
    }
}

/// `POST /api/tenants` : {id, dir, project?, version}.
async fn handle_add_tenant(proxy: Option<Arc<ProxyServer>>, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "JSON invalide"),
    };
    let (id, dir, version) = match (required_str(&data, "id"), required_str(&data, "dir"), required_str(&data, "version")) {
        (Ok(i), Ok(d), Ok(v)) => (i.to_string(), PathBuf::from(d.trim()), v.to_string()),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return r,
    };
    if !crate::tenant::is_valid_id(&id) {
        return json_err(400, "id invalide (1 a 32 minuscules, chiffres, tirets)");
    }
    let project = data
        .get("project")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(&id)
        .to_string();
    let path = crate::tenant::projects_path();
    let (task_id, task_project) = (id.clone(), project.clone());
    let added = tokio::task::spawn_blocking(move || {
        crate::tenant::add_from_dashboard(&path, &version, &task_id, &dir, &task_project)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let outcome = added.as_ref().map(|_| ()).map_err(Clone::clone);
    audit_tenant_action(&proxy, &id, "tenant.add".into(), &outcome).await;
    match added {
        Ok(backup) => json_ok(json!({ "ok": true, "project": project, "backup": backup })),
        Err(e) => projects_write_error(&e),
    }
}

/// `DELETE /api/tenants/<id>` : fenêtre macOS native obligatoire (ni la page ni un agent ne
/// peuvent la valider), puis effacement du coffre, du HOME isolé et de l'entrée.
async fn handle_remove_tenant(proxy: Option<Arc<ProxyServer>>, id: &str, version: Option<String>) -> Vec<u8> {
    let Some(version) = version else {
        return json_err(400, "version requise");
    };
    // Avant la fenêtre : on ne demande pas de confirmer un état que l'écran n'affiche plus.
    if version != crate::tenant::projects_version(&crate::tenant::projects_path()) {
        return json_err(409, crate::tenant::STALE_PROJECTS);
    }
    let def = match crate::tenant::load_from(&crate::tenant::projects_path()) {
        Ok(t) => match t.get(id) {
            Some(d) => d.clone(),
            None => return json_err(404, &format!("tenant {} inconnu", id)),
        },
        Err(e) => return json_err(500, &e),
    };
    let folders: Vec<String> = def.workspaces.iter().map(|w| format!("  {}", w.display())).collect();
    let message = format!(
        "Retirer le compte client \"{}\" de McpHub ?\n\nIRREVERSIBLE : tous ses secrets et jetons OAuth seront effaces du Trousseau, ainsi que son dossier isole. Le projet {} reste dans projects.json, sans tenant.\n\nDossiers :\n{}",
        id,
        def.project,
        folders.join("\n")
    );
    let refusal = match crate::proxy::ask_human(message, 120).await {
        crate::confirm_dialog::DialogAnswer::Allow => None,
        crate::confirm_dialog::DialogAnswer::Deny => Some("retrait refuse dans la fenetre de confirmation"),
        crate::confirm_dialog::DialogAnswer::Timeout => Some("pas de reponse dans la fenetre de confirmation : rien n'a ete efface"),
        crate::confirm_dialog::DialogAnswer::Unavailable => {
            Some("fenetre de confirmation macOS indisponible : utiliser McpHub tenant rm dans un Terminal")
        }
    };
    if let Some(reason) = refusal {
        audit_tenant_action(&proxy, id, "tenant.remove".into(), &Err(reason.to_string())).await;
        return json_err(403, reason);
    }
    reset_tenant(&proxy, id).await;
    let path = crate::tenant::projects_path();
    let task_id = id.to_string();
    let purged = tokio::task::spawn_blocking(move || crate::tenant::purge_tenant(&path, &task_id, Some(&version)))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let outcome = purged.as_ref().map(|_| ()).map_err(Clone::clone);
    audit_tenant_action(&proxy, id, "tenant.remove".into(), &outcome).await;
    match purged {
        Ok(report) => json_ok(json!({ "ok": true, "report": report })),
        Err(e) if e == crate::tenant::STALE_PROJECTS => json_err(409, &e),
        Err(e) => json_err(500, &e),
    }
}

/// `PUT /api/tenants/<id>/servers` : {servers: {serveur: [VARIABLES]}, shared: [serveurs], version}.
async fn handle_set_tenant_servers(proxy: Option<Arc<ProxyServer>>, id: &str, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "JSON invalide"),
    };
    let version = match required_str(&data, "version") {
        Ok(v) => v.to_string(),
        Err(r) => return r,
    };
    let mut servers: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    let Some(raw_servers) = data.get("servers").and_then(|s| s.as_object()) else {
        return json_err(400, "servers requis (objet serveur -> liste de variables)");
    };
    for (server, vars) in raw_servers {
        let Some(vars) = vars.as_array() else {
            return json_err(400, &format!("servers.{} : liste attendue", server));
        };
        let mut names = Vec::new();
        for v in vars {
            match v.as_str() {
                Some(name) => names.push(name.to_string()),
                None => return json_err(400, &format!("servers.{} : noms de variables attendus", server)),
            }
        }
        servers.insert(server.clone(), names);
    }
    let shared: Vec<String> = match data.get("shared").and_then(|s| s.as_array()) {
        Some(items) => match items.iter().map(|v| v.as_str().map(String::from)).collect::<Option<Vec<_>>>() {
            Some(list) => list,
            None => return json_err(400, "shared : noms de serveurs attendus"),
        },
        None => return json_err(400, "shared requis (liste)"),
    };
    let path = crate::tenant::projects_path();
    let task_id = id.to_string();
    let written = tokio::task::spawn_blocking(move || {
        let known = crate::tenant::configured_servers()?.into_keys().collect();
        crate::tenant::edit_projects(&path, &version, &format!("serveurs-tenant-{}", task_id), |json| {
            crate::tenant::set_servers(json, &task_id, &servers, &shared, &known)
        })
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let outcome = written.as_ref().map(|_| ()).map_err(Clone::clone);
    audit_tenant_action(&proxy, id, "tenant.servers_set".into(), &outcome).await;
    match written {
        Ok(backup) => {
            reset_tenant(&proxy, id).await;
            json_ok(json!({ "ok": true, "backup": backup }))
        }
        Err(e) => projects_write_error(&e),
    }
}

/// Secret déclaré par un serveur du tenant (les jetons OAuth ne s'écrivent pas ici).
fn declared_secret(id: &str, name: &str) -> Result<(), Vec<u8>> {
    if !crate::vault::is_valid_name(name) || name.starts_with(crate::oauth::VAULT_PREFIX) {
        return Err(json_err(400, "nom de secret invalide"));
    }
    let tenants = crate::tenant::load_from(&crate::tenant::projects_path()).map_err(|e| json_err(500, &e))?;
    let def = tenants.get(id).ok_or_else(|| json_err(404, &format!("tenant {} inconnu", id)))?;
    if def.credentials.values().any(|vars| vars.values().any(|n| n == name)) {
        Ok(())
    } else {
        Err(json_err(400, &format!("{} n'est pas un secret declare par les serveurs du tenant {}", name, id)))
    }
}

/// `PUT /api/tenants/<id>/secrets/<NOM>` : {value, verify?}. Écriture seule : la valeur part
/// vers le coffre du tenant et n'est jamais renvoyée ni journalisée.
async fn handle_set_tenant_secret(proxy: Option<Arc<ProxyServer>>, id: &str, name: &str, body: &str) -> Vec<u8> {
    if let Err(resp) = declared_secret(id, name) {
        return resp;
    }
    key_response(store_key_from_body(proxy, KeyTarget::Tenant(id.to_string()), name, body).await)
}

// ─── Connexion des services : jeton collé, code d'appareil, profils ───

/// Coffre qui reçoit un jeton : celui de McpHub (noms = variables) ou celui d'un tenant.
#[derive(Clone, Debug)]
enum KeyTarget {
    Hub,
    Tenant(String),
}

impl KeyTarget {
    fn tenant(&self) -> Option<&str> {
        match self {
            KeyTarget::Hub => None,
            KeyTarget::Tenant(id) => Some(id),
        }
    }
}

/// Emplacements d'un coffre (variable → noms) et noms déjà remplis.
struct KeySlots {
    slots: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    present: std::collections::BTreeSet<String>,
    /// Nom dans le coffre → serveurs qui le reçoivent (avis d'échéance aux agents).
    servers: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Lecture bloquante (fichiers, Trousseau). Compte McpHub : variables `${...}` référencées par
/// les serveurs de config.json ; tenant : secrets déclarés dans projects.json.
fn key_slots(target: &KeyTarget) -> Result<KeySlots, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut slots: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut servers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    match target {
        KeyTarget::Hub => {
            for (server, raw) in crate::tenant::configured_servers()? {
                for var in crate::tenant::referenced_vars(&raw) {
                    servers.entry(var.clone()).or_default().insert(server.clone());
                    slots.entry(var.clone()).or_default().insert(var);
                }
            }
            let mut present: BTreeSet<String> = crate::vault::list_names().into_iter().collect();
            present.extend(slots.keys().filter(|v| std::env::var(v.as_str()).is_ok()).cloned());
            Ok(KeySlots { slots, present, servers })
        }
        KeyTarget::Tenant(id) => {
            let tenants = crate::tenant::load_from(&crate::tenant::projects_path())?;
            let def = tenants.get(id).ok_or_else(|| format!("tenant {} inconnu", id))?;
            for (server, vars) in &def.credentials {
                for (var, name) in vars {
                    servers.entry(name.clone()).or_default().insert(server.clone());
                    slots.entry(var.clone()).or_default().insert(name.clone());
                }
            }
            let scope = crate::vault::Scope::Tenant(id.clone());
            Ok(KeySlots { slots, present: crate::vault::list_names_in(&scope).into_iter().collect(), servers })
        }
    }
}

/// Variable correspondant au nom visé, s'il est bien un emplacement du coffre.
fn slot_var(slots: &KeySlots, name: &str) -> Option<String> {
    if name.starts_with(crate::oauth::VAULT_PREFIX) || !crate::vault::is_valid_name(name) {
        return None;
    }
    slots.slots.iter().find(|(_, names)| names.contains(name)).map(|(var, _)| var.clone())
}

/// Échec d'un enregistrement de jeton : statut HTTP, message, et vrai si le service était
/// injoignable (le tableau de bord propose alors d'enregistrer sans vérifier).
struct KeyError {
    status: u16,
    message: String,
    unreachable: bool,
}

impl KeyError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), unreachable: false }
    }
}

fn key_response(result: Result<Value, KeyError>) -> Vec<u8> {
    match result {
        Ok(v) => json_ok(v),
        Err(e) => {
            let body = json!({ "error": e.message, "unreachable": e.unreachable }).to_string();
            let reason = match e.status {
                400 => "Bad Request",
                404 => "Not Found",
                413 => "Payload Too Large",
                422 => "Unprocessable Entity",
                502 => "Bad Gateway",
                _ => "Internal Server Error",
            };
            http_response(e.status, reason, "application/json", &body)
        }
    }
}

async fn store_key_from_body(
    proxy: Option<Arc<ProxyServer>>,
    target: KeyTarget,
    name: &str,
    body: &str,
) -> Result<Value, KeyError> {
    if body.len() > SECRET_BODY_MAX {
        return Err(KeyError::new(413, "valeur trop longue (16 Ko au plus)"));
    }
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let value = match data.get("value").and_then(|x| x.as_str()) {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => return Err(KeyError::new(400, "value requis (texte non vide)")),
    };
    let verify = data.get("verify").and_then(|v| v.as_bool()).unwrap_or(true);
    let manual_expiry = match data.get("expires_on").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        None => None,
        Some(date) => match crate::token_watch::parse_datetime(date) {
            Some(ms) if date.len() == 10 => Some(ms),
            _ => return Err(KeyError::new(400, "expires_on : date AAAA-MM-JJ attendue")),
        },
    };
    store_key(proxy, target, name, value, verify, manual_expiry).await
}

/// Range un jeton (collé ou obtenu par code d'appareil) : contrôle de forme, vérification
/// auprès du service, puis écriture du nom visé et des emplacements vides que le même jeton
/// (ou l'Account ID Cloudflare retrouvé) peut remplir. Jamais de valeur dans un message.
async fn store_key(
    proxy: Option<Arc<ProxyServer>>,
    target: KeyTarget,
    name: &str,
    value: String,
    verify: bool,
    manual_expiry: Option<u64>,
) -> Result<Value, KeyError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(KeyError::new(400, "valeur sur plusieurs lignes non prise en charge"));
    }
    let task_target = target.clone();
    let slots = tokio::task::spawn_blocking(move || key_slots(&task_target))
        .await
        .map_err(|e| KeyError::new(500, e.to_string()))?
        .map_err(|e| KeyError::new(400, e))?;
    let var = slot_var(&slots, name).ok_or_else(|| {
        KeyError::new(
            400,
            match &target {
                KeyTarget::Hub => format!("{} n'est attendu par aucun serveur de config.json", name),
                KeyTarget::Tenant(id) => format!("{} n'est pas un secret declare par les serveurs du tenant {}", name, id),
            },
        )
    })?;
    let connector = crate::connectors::for_var(&var);
    if let Some(c) = connector {
        crate::connectors::check_format(c, &value).map_err(|e| KeyError::new(400, e))?;
    }
    let verified = match (connector, verify) {
        (Some(c), true) => match crate::connectors::verify(c, &value).await {
            Ok(v) => Some(v),
            Err(crate::connectors::VerifyError::Refused(m)) => return Err(KeyError::new(422, m)),
            Err(crate::connectors::VerifyError::Unreachable(m)) => {
                return Err(KeyError { status: 502, message: m, unreachable: true })
            }
        },
        _ => None,
    };
    let writes = crate::connectors::plan_writes(
        name,
        connector,
        verified.as_ref().and_then(|v| v.account_id.as_deref()),
        &slots.slots,
        &slots.present,
    );
    let names: Vec<String> = writes.iter().map(|(n, _)| n.clone()).collect();
    let replaced = slots.present.contains(name);
    // Fiches d'échéance des jetons écrits (l'Account ID n'expire pas).
    let records: Vec<(String, String, Vec<String>)> = writes
        .iter()
        .filter(|(_, fill)| *fill == crate::connectors::Fill::Token)
        .map(|(slot, _)| {
            let slot_var = slots
                .slots
                .iter()
                .find(|(_, names)| names.contains(slot))
                .map(|(v, _)| v.clone())
                .unwrap_or_else(|| var.clone());
            let servers = slots.servers.get(slot).map(|s| s.iter().cloned().collect()).unwrap_or_default();
            (slot.clone(), slot_var, servers)
        })
        .collect();
    let service_expiry = verified.as_ref().and_then(|v| v.expires_at);
    let is_verified = verified.is_some();
    let task_target = target.clone();
    let written = tokio::task::spawn_blocking(move || {
        for (slot, fill) in &writes {
            let content = match fill {
                crate::connectors::Fill::Token => value.as_str(),
                crate::connectors::Fill::AccountId(id) => id.as_str(),
            };
            let done = match &task_target {
                KeyTarget::Tenant(id) => crate::vault::set_in(&crate::vault::Scope::Tenant(id.clone()), slot, content),
                KeyTarget::Hub => crate::vault::set(slot, content).map(|_| {
                    // Le démon résout ${NOM} depuis son environnement : la valeur sert dès maintenant.
                    std::env::set_var(slot, content);
                }),
            };
            done.map_err(|e| e.replace(value.as_str(), "***"))?;
        }
        let scope = match &task_target {
            KeyTarget::Hub => crate::vault::Scope::Hub,
            KeyTarget::Tenant(id) => crate::vault::Scope::Tenant(id.clone()),
        };
        for (slot, slot_var, servers) in records {
            let recorded = crate::token_watch::record(
                &scope,
                crate::token_watch::Recorded {
                    name: &slot,
                    var: &slot_var,
                    service: crate::connectors::for_var(&slot_var).map(|c| c.id),
                    servers,
                    verified: is_verified,
                    service_expiry,
                    manual_expiry,
                },
            );
            // Le jeton est rangé : une fiche non écrite ne fait que retarder l'alerte.
            if let Err(e) = recorded {
                eprintln!("[McpHub][TOKENS] fiche de {} non enregistree : {}", slot, e);
            }
        }
        Ok::<(), String>(())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    audit_dashboard_action(&proxy, target.tenant(), format!(
            "{} {}",
            if target.tenant().is_some() { "tenant.secret_set" } else { "secret_set" },
            names.join(",")
        ), &written).await;
    written.map_err(|e| KeyError::new(500, e))?;
    match &target {
        KeyTarget::Tenant(id) => reset_tenant(&proxy, id).await,
        KeyTarget::Hub => {
            if let Some(p) = &proxy {
                p.reload_config_now().await;
            }
        }
    }
    Ok(json!({
        "ok": true,
        "replaced": replaced,
        "written": names,
        "verified": verified.is_some(),
        "expires_at": service_expiry.or(manual_expiry),
        "detail": verified.map(|v| v.detail),
    }))
}

/// `PUT /api/secrets/<NOM>` : {value, verify?}. Jeton du compte McpHub, pour une variable
/// `${NOM}` attendue par un serveur de config.json.
async fn handle_set_hub_secret(proxy: Option<Arc<ProxyServer>>, name: &str, body: &str) -> Vec<u8> {
    key_response(store_key_from_body(proxy, KeyTarget::Hub, name, body).await)
}

/// Coffre visé par une requête : `tenant` absent = compte McpHub.
fn key_target(data: &Value) -> Result<KeyTarget, Vec<u8>> {
    match data.get("tenant").and_then(|t| t.as_str()) {
        None => Ok(KeyTarget::Hub),
        Some(t) if crate::tenant::is_valid_id(t) => Ok(KeyTarget::Tenant(t.to_string())),
        Some(_) => Err(json_err(400, "id de tenant invalide")),
    }
}

static DEVICE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// `POST /api/connect/device` : {name, tenant?}. Connexion GitHub par code d'appareil : le
/// jeton obtenu est rangé comme un jeton collé, sous `name`.
async fn handle_device_start(proxy: Option<Arc<ProxyServer>>, body: &str) -> Vec<u8> {
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let target = match key_target(&data) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let Some(name) = data.get("name").and_then(|n| n.as_str()).map(String::from) else {
        return json_err(400, "name requis");
    };
    let task_target = target.clone();
    let slots = match tokio::task::spawn_blocking(move || key_slots(&task_target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return json_err(400, &e),
        Err(e) => return json_err(500, &e.to_string()),
    };
    let is_github = slot_var(&slots, &name)
        .and_then(|var| crate::connectors::for_var(&var))
        .is_some_and(|c| c.id == "github");
    if !is_github {
        return json_err(400, &format!("{} n'est pas un jeton GitHub attendu par ce coffre", name));
    }
    let Some(client_id) = crate::connectors::github_client_id() else {
        return json_err(400, "aucune app GitHub configuree (settings.githubClientId) : utiliser le lien pre-rempli");
    };
    let start = match crate::connectors::github_device_start(&client_id).await {
        Ok(s) => s,
        Err(e) => return json_err(502, &e),
    };
    let id = format!("d{}", DEVICE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    let reply = json!({
        "ok": true,
        "id": id,
        "user_code": start.user_code,
        "verification_uri": start.verification_uri,
        "expires_in": start.expires_in,
    });
    crate::connectors::device_set(&id, crate::connectors::DeviceState::Pending);
    tokio::spawn(async move {
        let state = match crate::connectors::github_device_wait(start).await {
            Ok(token) => match store_key(proxy, target, &name, token, true, None).await {
                Ok(v) => crate::connectors::DeviceState::Done(
                    v.get("detail").and_then(|d| d.as_str()).unwrap_or("connected").to_string(),
                ),
                Err(e) => crate::connectors::DeviceState::Failed(e.message),
            },
            Err(e) => crate::connectors::DeviceState::Failed(e),
        };
        crate::connectors::device_set(&id, state);
    });
    json_ok(reply)
}

/// `GET /api/connect/device?id=...` : état de la connexion par code d'appareil.
fn handle_device_status(path: &str) -> Vec<u8> {
    let Some(id) = query_param(path, "id") else {
        return json_err(400, "id requis");
    };
    match crate::connectors::device_get(&id) {
        None => json_err(404, "connexion inconnue"),
        Some(crate::connectors::DeviceState::Pending) => json_ok(json!({ "state": "pending" })),
        Some(crate::connectors::DeviceState::Done(detail)) => json_ok(json!({ "state": "done", "detail": detail })),
        Some(crate::connectors::DeviceState::Failed(e)) => json_ok(json!({ "state": "failed", "error": e })),
    }
}

/// Profil de navigateur choisi pour un tenant (lecture de projects.json).
fn tenant_browser_profile(tenant: &str) -> Option<crate::connectors::BrowserProfile> {
    let raw = fs::read_to_string(crate::tenant::projects_path()).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    crate::connectors::tenant_browser(&json, tenant)
}

fn profile_json(p: &crate::connectors::BrowserProfile) -> Value {
    json!({ "app": p.app, "dir": p.dir, "name": p.name })
}

/// `POST /api/open` : {url, tenant?}. Ouvre une page (création de jeton, autorisation) dans le
/// profil de navigateur du tenant s'il en a un, sinon dans le navigateur par défaut.
async fn handle_open(body: &str) -> Vec<u8> {
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let Some(url) = data.get("url").and_then(|u| u.as_str()).map(String::from) else {
        return json_err(400, "url requis");
    };
    let target = match key_target(&data) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let opened = tokio::task::spawn_blocking(move || {
        let profile = target.tenant().and_then(tenant_browser_profile);
        crate::connectors::open_url(&url, profile.as_ref()).map(|_| profile)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match opened {
        Ok(profile) => json_ok(json!({ "ok": true, "profile": profile.as_ref().map(profile_json) })),
        Err(e) => json_err(400, &e),
    }
}

/// `GET /api/browsers` : profils de navigateur détectés sur ce Mac.
async fn handle_browsers() -> Vec<u8> {
    let profiles = tokio::task::spawn_blocking(crate::connectors::browser_profiles).await.unwrap_or_default();
    json_ok(json!({ "profiles": profiles.iter().map(profile_json).collect::<Vec<_>>() }))
}

/// `PUT /api/tenants/<id>/browser` : {app?, profile?, version}. Sans app : navigateur par défaut.
async fn handle_set_tenant_browser(proxy: Option<Arc<ProxyServer>>, id: &str, body: &str) -> Vec<u8> {
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let version = match required_str(&data, "version") {
        Ok(v) => v.to_string(),
        Err(resp) => return resp,
    };
    let app = data.get("app").and_then(|a| a.as_str()).map(String::from);
    let dir = data.get("profile").and_then(|a| a.as_str()).map(String::from);
    let task_id = id.to_string();
    let saved = tokio::task::spawn_blocking(move || {
        let profile = match (app, dir) {
            (Some(app), Some(dir)) => Some(
                crate::connectors::browser_profiles()
                    .into_iter()
                    .find(|p| p.app == app && p.dir == dir)
                    .ok_or("profil de navigateur introuvable sur ce Mac")?,
            ),
            _ => None,
        };
        crate::tenant::edit_projects(&crate::tenant::projects_path(), &version, &format!("navigateur-{}", task_id), |json| {
            crate::connectors::set_tenant_browser(json, &task_id, profile.as_ref())
        })
        .map(|_| ())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    audit_tenant_action(&proxy, id, "tenant.browser_set".into(), &saved).await;
    match saved {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => projects_write_error(&e),
    }
}

/// `POST /api/oauth/manual` : {url}. Adresse de retour recopiée depuis le navigateur d'un client
/// à distance (le sien n'atteint pas localhost:24680).
fn handle_oauth_manual(body: &str) -> Vec<u8> {
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let Some(url) = data.get("url").and_then(|u| u.as_str()) else {
        return json_err(400, "url requis");
    };
    match crate::oauth::parse_pasted_callback(url) {
        Err(e) => json_err(400, &e),
        Ok((state, result)) => {
            if crate::oauth::complete_callback(&state, result) {
                json_ok(json!({ "ok": true }))
            } else {
                json_err(400, "connexion inconnue ou expiree : relancer Connect")
            }
        }
    }
}

/// `DELETE /api/tenants/<id>/secrets/<NOM>`.
async fn handle_delete_tenant_secret(proxy: Option<Arc<ProxyServer>>, id: &str, name: &str) -> Vec<u8> {
    if let Err(resp) = declared_secret(id, name) {
        return resp;
    }
    let scope = crate::vault::Scope::Tenant(id.to_string());
    let task_name = name.to_string();
    let deleted = tokio::task::spawn_blocking(move || crate::vault::delete_in(&scope, &task_name))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let outcome = deleted.as_ref().map(|_| ()).map_err(Clone::clone);
    audit_tenant_action(&proxy, id, format!("tenant.secret_delete {}", name), &outcome).await;
    match deleted {
        Ok(existed) => {
            reset_tenant(&proxy, id).await;
            json_ok(json!({ "ok": true, "deleted": existed }))
        }
        Err(e) => json_err(500, &e),
    }
}

/// `POST /api/tenants/<id>/check` : chaque instance peut-elle démarrer ? (`McpHub tenant check`)
async fn handle_check_tenant(id: &str) -> Vec<u8> {
    let task_id = id.to_string();
    let checked = tokio::task::spawn_blocking(move || {
        let tenants = crate::tenant::load_from(&crate::tenant::projects_path())?;
        let def = tenants.get(&task_id).ok_or_else(|| format!("tenant {} inconnu", task_id))?;
        Ok::<_, String>(
            def.credentials
                .keys()
                .map(|server| match crate::tenant::check_server(def, server) {
                    Ok(message) => json!({ "server": server, "ok": true, "message": message }),
                    Err(e) => json!({ "server": server, "ok": false, "message": e }),
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match checked {
        Ok(results) => json_ok(json!({ "results": results })),
        Err(e) => json_err(404, &e),
    }
}

/// Routes `/api/tenants/<id>[/...]`.
async fn route_tenant(req: &HttpRequest, rest: &str, proxy: Option<Arc<ProxyServer>>) -> Vec<u8> {
    let parts: Vec<String> = rest.split('/').map(urldecode).collect();
    let id = parts[0].as_str();
    if !crate::tenant::is_valid_id(id) {
        return json_err(400, "id de tenant invalide");
    }
    let tail: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
    match (&req.method[..], tail.as_slice()) {
        ("DELETE", []) => handle_remove_tenant(proxy, id, query_param(&req.path, "version")).await,
        ("PUT", ["servers"]) => handle_set_tenant_servers(proxy, id, &req.body).await,
        ("POST", ["check"]) => handle_check_tenant(id).await,
        ("PUT", ["secrets", name]) => handle_set_tenant_secret(proxy, id, name, &req.body).await,
        ("PUT", ["browser"]) => handle_set_tenant_browser(proxy, id, &req.body).await,
        ("DELETE", ["secrets", name]) => handle_delete_tenant_secret(proxy, id, name).await,
        (_, [] | ["servers"] | ["check"] | ["secrets", _] | ["browser"]) => json_err(405, "Method not allowed"),
        _ => json_err(404, "Not found"),
    }
}

// ─── Router ──────────────────────────────────────────────────

async fn route(
    req: &HttpRequest,
    proxy: Option<Arc<ProxyServer>>,
    sse: Option<Arc<SseManager>>,
) -> Vec<u8> {
    let path = req.path.split('?').next().unwrap_or(&req.path);

    if req.method == "OPTIONS" {
        return http_response(204, "No Content", "text/plain", "");
    }

    match (&req.method[..], path) {
        ("GET", "/") => http_response_with(
            200,
            "OK",
            "text/html; charset=utf-8",
            DASHBOARD_HTML,
            "X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nCache-Control: no-store\r\n",
        ),
        ("GET", "/oauth/callback") => {
            // Retour du navigateur après connexion OAuth d'un serveur distant.
            let headers = "Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n";
            let state = query_param(&req.path, "state").unwrap_or_default();
            let outcome = match (query_param(&req.path, "code"), query_param(&req.path, "error")) {
                (Some(code), _) => Ok(code),
                (None, Some(err)) => Err(err),
                _ => Err("reponse incomplete".to_string()),
            };
            let ok = outcome.is_ok();
            let known = crate::oauth::complete_callback(&state, outcome);
            let page = match (known, ok) {
                (true, true) => OauthPage::Connected,
                (true, false) => OauthPage::Denied,
                (false, _) => OauthPage::Expired,
            };
            let french = req.headers.get("accept-language").is_some_and(|langs| prefers_french(langs));
            http_response_with(
                if known { 200 } else { 400 },
                if known { "OK" } else { "Bad Request" },
                "text/html; charset=utf-8",
                &oauth_callback_page(page, french),
                headers,
            )
        }
        ("POST", "/api/oauth/login") => {
            let body = serde_json::from_str::<Value>(&req.body).unwrap_or(Value::Null);
            let server = body.get("server").and_then(|s| s.as_str()).map(String::from);
            let tenant = body.get("tenant").and_then(|s| s.as_str()).map(String::from);
            match (server, tenant) {
                (None, _) => json_err(400, "server requis"),
                (Some(server), _) if crate::tenant::is_instance_name(&server) => {
                    json_err(400, "nom de serveur invalide : passer le tenant dans \"tenant\"")
                }
                (Some(server), Some(tenant)) => tenant_oauth_login(&server, &tenant).await,
                (Some(server), None) => {
                    let cfg = crate::config::auto_detect();
                    match cfg.servers.get(&server).and_then(|s| s.url.clone()) {
                        None => json_err(404, "serveur distant inconnu (pas d'url dans la config)"),
                        Some(url) => {
                            tokio::spawn(async move {
                                if let Err(e) = crate::oauth::interactive_login(&server, &url, None).await {
                                    eprintln!("[McpHub][OAUTH] {} : echec de connexion : {}", server, e);
                                }
                            });
                            http_response(202, "Accepted", "application/json", "{\"ok\":true}")
                        }
                    }
                }
            }
        }
        ("POST", "/api/oauth/logout") => {
            let body = serde_json::from_str::<Value>(&req.body).unwrap_or(Value::Null);
            let server = body.get("server").and_then(|s| s.as_str()).map(String::from);
            let tenant = body.get("tenant").and_then(|s| s.as_str()).map(String::from);
            let target = match (server, tenant) {
                (None, _) => Err(json_err(400, "server requis")),
                (Some(server), _) if crate::tenant::is_instance_name(&server) => {
                    Err(json_err(400, "nom de serveur invalide : passer le tenant dans \"tenant\""))
                }
                (Some(server), Some(tenant)) => tenant_server(&server, &tenant)
                    .map(|_| crate::tenant::instance_name(&server, &tenant))
                    .map_err(|e| json_err(400, &e)),
                (Some(server), None) => Ok(server),
            };
            match target {
                Err(resp) => resp,
                Ok(name) => match crate::oauth::forget(&name) {
                    Ok(_) => http_response(200, "OK", "application/json", "{\"ok\":true}"),
                    Err(e) => json_err(500, &e),
                },
            }
        }
        ("POST", "/api/oauth/manual") => handle_oauth_manual(&req.body),
        ("POST", "/api/connect/device") => handle_device_start(proxy, &req.body).await,
        ("POST", "/api/open") => handle_open(&req.body).await,
        ("GET", "/api/browsers") => handle_browsers().await,
        ("GET", "/api/token-alerts") => {
            let alerts = tokio::task::spawn_blocking(crate::token_watch::alerts).await.unwrap_or_default();
            json_ok(json!({ "alerts": alerts.iter().map(|a| a.to_json()).collect::<Vec<_>>() }))
        }
        ("GET", "/api/tenants") => handle_get_tenants(proxy).await,
        ("POST", "/api/tenants") => handle_add_tenant(proxy, &req.body).await,
        ("GET", "/login") => {
            // Lien à usage unique créé par `McpHub dashboard` : remet le secret de session au
            // tableau de bord (sessionStorage, propre à cette origine), puis efface l'URL.
            let headers = "X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n";
            let ok = query_param(&req.path, "nonce")
                .map(|n| crate::http_guard::consume_login_nonce(&n))
                .unwrap_or(false);
            if ok {
                let body = format!(
                    "<!doctype html><meta charset=utf-8><title>McpHub</title><script>try{{sessionStorage.setItem('mcphub_session','{}')}}catch(e){{}}location.replace('/');</script>",
                    crate::http_guard::session_secret()
                );
                http_response_with(200, "OK", "text/html; charset=utf-8", &body, headers)
            } else {
                http_response_with(
                    403,
                    "Forbidden",
                    "text/html; charset=utf-8",
                    "<!doctype html><meta charset=utf-8><title>McpHub</title><p style='font-family:system-ui;padding:40px'>Lien expire ou deja utilise. Relancer <code>McpHub dashboard</code>.</p>",
                    headers,
                )
            }
        }
        ("GET", "/api/registry/search") => {
            let q = query_param(&req.path, "q").unwrap_or_default();
            let cursor = query_param(&req.path, "cursor");
            let limit = query_param(&req.path, "limit").and_then(|l| l.parse().ok()).unwrap_or(30);
            match crate::registry::search(&q, cursor.as_deref(), limit).await {
                Ok(v) => json_ok(v),
                Err(e) => json_err(502, &format!("Registre MCP : {}", e)),
            }
        }
        ("GET", "/api/registry/server") => match query_param(&req.path, "name") {
            None => json_err(400, "name requis"),
            Some(name) => match crate::registry::details(&name).await {
                Ok(v) => json_ok(v),
                Err(e) => json_err(502, &format!("Registre MCP : {}", e)),
            },
        },
        ("GET", "/api/registry/status") => json_ok(crate::registry::sync_status()),
        ("POST", "/api/registry/sync") => {
            crate::registry::ensure_sync();
            json_ok(crate::registry::sync_status())
        }
        ("POST", "/api/registry/install") => {
            let body: Value = serde_json::from_str(&req.body).unwrap_or(json!({}));
            match body.get("name").and_then(|v| v.as_str()) {
                None => json_err(400, "name requis"),
                Some(name) => {
                    let request = crate::registry::InstallRequest {
                        name: name.to_string(),
                        option: body.get("option").and_then(|v| v.as_str()).map(String::from),
                        values: body.get("values").and_then(|v| v.as_object()).cloned().unwrap_or_default(),
                        local_name: body.get("localName").and_then(|v| v.as_str()).map(String::from),
                        by: "dashboard".into(),
                    };
                    match crate::registry::install(request).await {
                        Ok(v) => {
                            if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                                if let Some(p) = &proxy {
                                    p.reload_config_now().await;
                                }
                            }
                            json_ok(v)
                        }
                        Err(e) => json_err(400, &e),
                    }
                }
            }
        }
        ("POST", "/api/registry/uninstall") => {
            let body: Value = serde_json::from_str(&req.body).unwrap_or(json!({}));
            match body.get("server").and_then(|v| v.as_str()) {
                None => json_err(400, "server requis"),
                Some(server) => match crate::registry::uninstall(server) {
                    Ok(removed) => {
                        if let Some(p) = &proxy {
                            p.reload_config_now().await;
                        }
                        json_ok(json!({"ok": removed}))
                    }
                    Err(e) => json_err(500, &e),
                },
            }
        }
        ("GET", "/api/servers") => handle_get_servers(),
        ("POST", "/api/servers") => handle_add_server(&req.body),
        ("GET", "/api/settings") => handle_get_settings(),
        ("GET", "/api/metrics") => handle_get_metrics(proxy, sse).await,
        ("PUT", "/api/settings") => handle_update_settings(&req.body),
        ("POST", "/api/generate") => handle_generate().await,
        _ => {
            if let Some(rest) = path.strip_prefix("/api/tenants/") {
                return route_tenant(req, rest, proxy).await;
            }
            if path == "/api/connect/device" && req.method == "GET" {
                return handle_device_status(&req.path);
            }
            if let Some(name) = path.strip_prefix("/api/secrets/") {
                return match &req.method[..] {
                    "PUT" => handle_set_hub_secret(proxy, &urldecode(name), &req.body).await,
                    _ => json_err(405, "Method not allowed"),
                };
            }
            if let Some(rest) = path.strip_prefix("/api/servers/") {
                if (rest.ends_with("/toggle") || rest.ends_with("/repair")) && req.method != "POST" {
                    json_err(405, "Method not allowed")
                } else if let Some(name) = rest.strip_suffix("/toggle") {
                    let decoded = urldecode(name);
                    handle_toggle_server(&decoded, &req.body)
                } else if let Some(name) = rest.strip_suffix("/repair") {
                    let decoded = urldecode(name);
                    handle_repair_server(&decoded).await
                } else {
                    let decoded = urldecode(rest);
                    match &req.method[..] {
                        "PUT" => handle_update_server(&decoded, &req.body),
                        "DELETE" => handle_delete_server(&decoded),
                        _ => json_err(405, "Method not allowed"),
                    }
                }
            } else {
                json_err(404, "Not found")
            }
        }
    }
}

fn urldecode(s: &str) -> String {
    // Octets décodés puis UTF-8 : "%C3%A9" donne "é", pas deux caractères.
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            }
        } else if c == '+' {
            bytes.push(b' ');
        } else {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

// ─── Server Entry Point ─────────────────────────────────────

/// Start dashboard only (no SSE, no proxy). For `McpHub dashboard` command.
pub async fn start_dashboard() {
    // Démon déjà lancé : on ouvre simplement un lien de connexion à usage unique.
    let addr = format!("127.0.0.1:{}", crate::instance::port());
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        open_login_page();
        println!("Tableau de bord ouvert dans le navigateur (lien valable 2 minutes).");
        return;
    }
    start_http(None, None, true).await;
}

fn open_login_page() {
    let Some(nonce) = crate::http_guard::create_login_nonce() else {
        eprintln!("[McpHub] Impossible de creer le lien de connexion.");
        return;
    };
    let url = format!("http://127.0.0.1:{}/login?nonce={}", crate::instance::port(), nonce);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/c", "start", &url]).spawn();
}

/// Start full server: dashboard + SSE transport. For `McpHub serve` and default mode.
pub async fn start_server(proxy: Arc<ProxyServer>) {
    tokio::spawn(crate::token_watch::run_loop());
    start_http(Some(proxy), Some(Arc::new(SseManager::new())), false).await;
}

async fn start_http(
    proxy: Option<Arc<ProxyServer>>,
    sse: Option<Arc<SseManager>>,
    open_browser: bool,
) {
    let addr = format!("127.0.0.1:{}", crate::instance::port());
    let addr = addr.as_str();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[McpHub] Failed to bind {}: {}", addr, e);
            eprintln!("[McpHub] Is another instance running?");
            return;
        }
    };

    if proxy.is_some() {
        eprintln!("[McpHub][HTTP] Server ready on http://{}", addr);
        eprintln!("[McpHub][SSE]  Cursor endpoint: http://{}/sse", addr);
    } else {
        eprintln!("[dashboard] Running on http://{}", addr);
    }

    if open_browser {
        open_login_page();
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let proxy_clone = proxy.clone();
        let sse_clone = sse.clone();

        tokio::spawn(async move {
            handle_connection(stream, proxy_clone, sse_clone).await;
        });
    }
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    proxy: Option<Arc<ProxyServer>>,
    sse: Option<Arc<SseManager>>,
) {
    // Add CORS OPTIONS handler
    let mut buf = vec![0u8; 65536];
    let mut total_read =
        match tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut buf)).await
        {
            Ok(Ok(n)) if n > 0 => n,
            _ => return, // Timeout or read error: drop connection
        };

    // Check if it's an OPTIONS request early
    if total_read >= 7 && &buf[..7] == b"OPTIONS" {
        // Pas d'en-têtes CORS : aucun site tiers n'est autorisé à appeler le hub.
        let resp = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(resp).await;
        return;
    }

    // Find end of headers
    let mut body_offset = 0;
    for i in 0..total_read.saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            body_offset = i + 4;
            break;
        } else if i < total_read.saturating_sub(1) && &buf[i..i + 2] == b"\n\n" {
            body_offset = i + 2;
            break;
        }
    }

    if body_offset > 0 {
        // Find Content-Length
        let headers_str = String::from_utf8_lossy(&buf[..body_offset]);
        let mut content_length: usize = 0;
        for line in headers_str.lines() {
            let lower = line.to_lowercase();
            if let Some(val) = lower.strip_prefix("content-length:") {
                content_length = val.trim().parse().unwrap_or(0);
                break;
            }
        }

        // Corps limité à 16 Mo : un Content-Length géant ne doit pas faire planter le démon.
        if content_length > 16 * 1024 * 1024 {
            let resp = json_err(413, "Payload Too Large");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }
        // Read the rest of the body if needed
        let target_size = body_offset + content_length;
        if target_size > buf.len() {
            buf.resize(target_size, 0);
        }
        while total_read < target_size {
            let n = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                stream.read(&mut buf[total_read..]),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => return, // Timeout or read error
            };
            total_read += n;
        }
    }

    let raw = String::from_utf8_lossy(&buf[..total_read]).to_string();

    let req = match parse_request(&raw) {
        Some(r) => r,
        None => return,
    };

    let path = req.path.split('?').next().unwrap_or(&req.path).to_string();

    let auth_token = get_auth_token();
    let expected_auth = format!("Bearer {}", auth_token);

    // Anti DNS rebinding et anti requêtes envoyées par des sites tiers.
    if !crate::http_guard::host_allowed(req.headers.get("host").map(|s| s.as_str()), crate::instance::port())
        || !crate::http_guard::origin_allowed(
            req.headers.get("origin").map(|s| s.as_str()),
            crate::instance::port(),
        )
    {
        eprintln!(
            "[McpHub][SECURITY] Requete refusee (Host={:?}, Origin={:?}) {} {}",
            req.headers.get("host"),
            req.headers.get("origin"),
            req.method,
            path
        );
        let resp = json_err(403, "Forbidden");
        let _ = stream.write_all(&resp).await;
        let _ = stream.shutdown().await;
        return;
    }

    // Toute l'API du tableau de bord exige le jeton (Bearer) ou le cookie de session.
    let query_session = query_param(&req.path, "session");
    if path.starts_with("/api/")
        && !crate::http_guard::api_authorized(&req.headers, query_session.as_deref(), &auth_token)
    {
        let resp = json_err(401, "Unauthorized");
        let _ = stream.write_all(&resp).await;
        let _ = stream.shutdown().await;
        return;
    }

    // SSE endpoint: long-lived connection, don't close
    if path == "/sse" && req.method == "GET" {
        let auth = req
            .headers
            .get("authorization")
            .map(|s| s.as_str())
            .unwrap_or("");
        if !crate::http_guard::token_matches(auth, &expected_auth) {
            let resp = json_err(401, "Unauthorized");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }

        let profile = match requested_profile(&req.path) {
            Ok(p) => p,
            Err(resp) => {
                let _ = stream.write_all(&resp).await;
                let _ = stream.shutdown().await;
                return;
            }
        };
        if let Some(sse_mgr) = &sse {
            sse_mgr.handle_connect(stream, profile).await;
            return; // Connection handled, don't close
        } else {
            let resp = json_err(503, "SSE not available in dashboard-only mode");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }
    }

    let is_mcp_path = path == "/mcp" || path.starts_with("/mcp/");

    // Streamable HTTP : pas de flux serveur ni de session à fermer (mode sans état).
    if is_mcp_path && (req.method == "GET" || req.method == "DELETE") {
        let resp = http_response_with(
            405,
            "Method Not Allowed",
            "application/json",
            "{\"error\":\"Streamable HTTP sans etat : utiliser POST\"}",
            "Allow: POST\r\n",
        );
        let _ = stream.write_all(&resp).await;
        let _ = stream.shutdown().await;
        return;
    }

    // Streamable HTTP endpoint: Cursor V2 first POSTs JSON-RPC to the configured URL.
    // /mcp (et /mcp/<profil>) est l'adresse standard ; /sse en POST reste accepté.
    if (path == "/sse" || is_mcp_path) && req.method == "POST" {
        let auth = req
            .headers
            .get("authorization")
            .map(|s| s.as_str())
            .unwrap_or("");
        if !crate::http_guard::token_matches(auth, &expected_auth) {
            let resp = json_err(401, "Unauthorized");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }

        let response = if let Some(proxy_ref) = &proxy {
            match requested_profile(&req.path) {
                Ok(profile) => handle_streamable_http_message(&req.body, proxy_ref, profile).await,
                Err(resp) => resp,
            }
        } else {
            json_err(503, "MCP proxy not available in dashboard-only mode")
        };
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
        return;
    }

    // Message endpoint: process JSON-RPC via SSE session
    if path == "/message" && req.method == "POST" {
        let auth = req
            .headers
            .get("authorization")
            .map(|s| s.as_str())
            .unwrap_or("");
        if !crate::http_guard::token_matches(auth, &expected_auth) {
            let resp = json_err(401, "Unauthorized");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }

        let response = if let (Some(proxy_ref), Some(sse_mgr)) = (&proxy, &sse) {
            if let Some(session_id) = extract_session_id(&req.path) {
                sse_mgr
                    .handle_message(&session_id, &req.body, proxy_ref)
                    .await
            } else {
                json_err(400, "Missing sessionId parameter")
            }
        } else {
            json_err(503, "SSE not available in dashboard-only mode")
        };
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
        return;
    }

    if path == "/api/logs-stream" && req.method == "GET" {
        let headers = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: keep-alive\r\n\
             \r\n";
        if stream.write_all(headers.as_bytes()).await.is_err() {
            return;
        }

        let log_path = config_dir().join("mcphub.log");
        if !log_path.exists() {
            let _ = stream
                .write_all(b"event: message\ndata: {\"error\": \"Log file not found\"}\n\n")
                .await;
            return;
        }

        if let Ok(file) = std::fs::File::open(&log_path) {
            use std::io::{BufRead, Seek, SeekFrom};
            let mut reader = std::io::BufReader::new(file);
            let mut pos = reader.seek(SeekFrom::End(0)).unwrap_or(0);

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        if reader.seek(SeekFrom::Start(pos)).is_err() {
                            break;
                        }
                    }
                    Ok(len) => {
                        pos += len as u64;
                        let line_trim = line.trim();
                        if !line_trim.is_empty() {
                            let json_msg = serde_json::json!({ "line": line_trim });
                            let event =
                                format!("event: message\ndata: {}\n\n", json_msg);
                            if stream.write_all(event.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        return;
    }

    // Normal dashboard routes
    let response = route(&req, proxy, sse).await;
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

// ─── Embedded HTML ───────────────────────────────────────────

const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");
const OAUTH_CALLBACK_HTML: &str = include_str!("../static/oauth-callback.html");

#[cfg(test)]
mod oauth_page_tests {
    use super::*;

    #[test]
    fn french_only_when_first_language_is_french() {
        assert!(prefers_french("fr-FR,fr;q=0.9,en;q=0.8"));
        assert!(prefers_french("fr"));
        assert!(!prefers_french("en-US,en;q=0.9,fr;q=0.8"));
        assert!(!prefers_french("fy-NL"));
        assert!(!prefers_french(""));
    }

    #[test]
    fn every_page_is_fully_rendered() {
        for page in [OauthPage::Connected, OauthPage::Denied, OauthPage::Expired] {
            for french in [true, false] {
                let html = oauth_callback_page(page, french);
                assert!(!html.contains("{{"), "repère non remplacé");
                assert!(html.contains(if french { "lang=\"fr\"" } else { "lang=\"en\"" }));
            }
        }
        assert!(oauth_callback_page(OauthPage::Connected, true).contains("Connexion réussie"));
        assert!(oauth_callback_page(OauthPage::Connected, false).contains("You're connected"));
        assert!(oauth_callback_page(OauthPage::Expired, false).contains("McpHub oauth login SERVER"));
    }
}

#[cfg(test)]
mod cache_summary_tests {
    use super::*;

    #[test]
    fn summary_keeps_tool_names_sizes_and_errors() {
        let tools = json!([{ "name": "deploy", "inputSchema": {} }, { "name": "rollback" }, { "title": "sans nom" }]);
        let cache = json!({
            "servers": { "cf": tools.clone() },
            "errors": { "broken": "spawn failed", "ignored": 42 }
        });
        let summary = summarize_cache(&cache);
        let cf = &summary.servers["cf"];
        assert_eq!(cf.tool_names, vec!["deploy".to_string(), "rollback".to_string()]);
        assert_eq!(cf.schema_tokens, tools.to_string().len() / 4);
        assert_eq!(summary.errors.get("broken").map(String::as_str), Some("spawn failed"));
        assert!(!summary.errors.contains_key("ignored"));
    }

    #[test]
    fn empty_cache_gives_empty_summary() {
        let summary = summarize_cache(&json!({}));
        assert!(summary.servers.is_empty());
        assert!(summary.errors.is_empty());
    }
}
