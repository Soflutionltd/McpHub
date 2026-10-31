//! Embedded web dashboard for McpHub.
//! Serves HTML + JSON API on http://127.0.0.1:24680
//! Zero external dependencies — uses tokio::net::TcpListener directly.

use crate::protocol::JsonRpcRequest;
use crate::proxy::ProxyServer;
use crate::sse::{extract_session_id, SseManager};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
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

fn read_cache() -> Option<Value> {
    let path = cache_path();
    if !path.exists() {
        return None;
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
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
    let cache = read_cache();
    let servers_obj = config
        .get("mcpServers")
        .or_else(|| config.get("servers"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let cached_servers = cache
        .as_ref()
        .and_then(|c| c.get("servers"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let cached_errors = cache
        .as_ref()
        .and_then(|c| c.get("errors"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut result: Vec<Value> = Vec::new();
    let mut names: Vec<String> = servers_obj.keys().cloned().collect();
    names.sort();

    for name in &names {
        let srv = &servers_obj[name];
        let cached = cached_servers.get(name);
        let tools: Vec<String> = cached
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let tool_count = tools.len();

        let error_msg = cached_errors
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or("");

        result.push(json!({
            "name": name,
            "command": srv.get("command").and_then(|v| v.as_str()).unwrap_or(""),
            "args": srv.get("args").unwrap_or(&json!([])),
            "env": srv.get("env").unwrap_or(&json!({})),
            "disabled": srv.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            "tools": tool_count,
            "schemaTokens": cached.map(|c| c.to_string().len() / 4).unwrap_or(0),
            "url": srv.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            "toolNames": tools,
            "status": if cached.is_some() { "cached" } else if !error_msg.is_empty() { "error" } else { "uncached" },
            "error": error_msg
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
            let server = serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("server").and_then(|s| s.as_str()).map(String::from));
            match server {
                None => json_err(400, "server requis"),
                Some(server) => {
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
            match serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("server").and_then(|s| s.as_str()).map(String::from))
            {
                None => json_err(400, "server requis"),
                Some(server) => match crate::oauth::forget(&server) {
                    Ok(_) => http_response(200, "OK", "application/json", "{\"ok\":true}"),
                    Err(e) => json_err(500, &e),
                },
            }
        }
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
            if path.starts_with("/api/servers/") {
                let rest = &path["/api/servers/".len()..];
                if (rest.ends_with("/toggle") || rest.ends_with("/repair")) && req.method != "POST" {
                    json_err(405, "Method not allowed")
                } else if rest.ends_with("/toggle") {
                    let name = &rest[..rest.len() - "/toggle".len()];
                    let decoded = urldecode(name);
                    handle_toggle_server(&decoded, &req.body)
                } else if rest.ends_with("/repair") {
                    let name = &rest[..rest.len() - "/repair".len()];
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
                                format!("event: message\ndata: {}\n\n", json_msg.to_string());
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
