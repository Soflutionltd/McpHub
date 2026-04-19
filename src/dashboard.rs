//! Embedded web dashboard for McpHub.
//! Serves HTML + JSON API on http://127.0.0.1:24680
//! Zero external dependencies — uses tokio::net::TcpListener directly.

use crate::log_store::level_passes;
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
    dirs::home_dir().unwrap_or_default().join(".McpHub")
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
        if !token.is_empty() {
            return token;
        }
    }
    
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let token = format!("mcphub_{:x}", nanos);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    
    let _ = fs::write(&path, &token);
    
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(mut perms) = fs::metadata(&path).map(|m| m.permissions()) {
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&path, perms);
        }
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

fn read_config() -> Value {
    let path = config_path();
    if !path.exists() {
        return json!({"mcpServers": {}, "settings": {"mode": "discover", "idleTimeout": 300}});
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"mcpServers": {}, "settings": {}}))
}

fn save_config(config: &Value) -> bool {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    serde_json::to_string_pretty(config)
        .ok()
        .and_then(|json| fs::write(&path, json).ok())
        .is_some()
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

    Some(HttpRequest { method, path, headers, body })
}

fn http_response(status: u16, status_text: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, PUT, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n{}",
        status, status_text, content_type, body.len(), body
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

// ─── API Handlers ────────────────────────────────────────────

fn handle_get_servers() -> Vec<u8> {
    let config = read_config();
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

        let error_msg = cached_errors.get(name).and_then(|v| v.as_str()).unwrap_or("");

        result.push(json!({
            "name": name,
            "command": srv.get("command").and_then(|v| v.as_str()).unwrap_or(""),
            "args": srv.get("args").unwrap_or(&json!([])),
            "env": srv.get("env").unwrap_or(&json!({})),
            "disabled": srv.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            "tools": tool_count,
            "toolNames": tools,
            "status": if cached.is_some() { "cached" } else if !error_msg.is_empty() { "error" } else { "uncached" },
            "error": error_msg
        }));
    }

    let total_tools: usize = result.iter().map(|s| s["tools"].as_u64().unwrap_or(0) as usize).sum();

    json_ok(json!({
        "servers": result,
        "settings": config.get("settings").unwrap_or(&json!({})),
        "totalTools": total_tools,
        "cacheExists": cache.is_some()
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

    let mut config = read_config();
    let key = if config.get("servers").is_some() { "servers" } else { "mcpServers" };
    if config.get(key).is_none() {
        config[key] = json!({});
    }
    config[key][&name] = json!({
        "command": command,
        "args": args,
        "env": env
    });

    if save_config(&config) {
        json_ok(json!({"ok": true, "message": "Server added"}))
    } else {
        json_err(500, "Failed to save config")
    }
}

fn handle_update_server(name: &str, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };

    let mut config = read_config();
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() { "servers" } else { "mcpServers" };
    let servers = match config.get_mut(key).and_then(|v| v.as_object_mut()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };

    if !servers.contains_key(name) {
        return json_err(404, "Server not found");
    }

    let new_name = data
        .get("newName")
        .and_then(|v| v.as_str())
        .unwrap_or(name);

    if new_name != name {
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

    if save_config(&config) {
        json_ok(json!({"ok": true}))
    } else {
        json_err(500, "Failed to save config")
    }
}

fn handle_delete_server(name: &str) -> Vec<u8> {
    let mut config = read_config();
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() { "servers" } else { "mcpServers" };
    let servers = match config.get_mut(key).and_then(|v| v.as_object_mut()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };
    if servers.remove(name).is_none() {
        return json_err(404, "Server not found");
    }
    if save_config(&config) {
        json_ok(json!({"ok": true}))
    } else {
        json_err(500, "Failed to save config")
    }
}

fn handle_toggle_server(name: &str, body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let disabled = data.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut config = read_config();
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() { "servers" } else { "mcpServers" };
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
    if save_config(&config) {
        json_ok(json!({"ok": true, "disabled": disabled}))
    } else {
        json_err(500, "Failed to save config")
    }
}

fn handle_get_settings() -> Vec<u8> {
    let config = read_config();
    let settings = config
        .get("settings")
        .cloned()
        .unwrap_or(json!({"mode": "discover", "idleTimeout": 300}));
    json_ok(settings)
}

async fn handle_get_metrics(proxy: Option<Arc<ProxyServer>>, sse: Option<Arc<SseManager>>) -> Vec<u8> {
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

fn parse_query(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(idx) = path.find('?') {
        for pair in path[idx + 1..].split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                out.insert(urldecode(k), urldecode(v));
            } else if !pair.is_empty() {
                out.insert(urldecode(pair), String::new());
            }
        }
    }
    out
}

async fn handle_get_logs(proxy: Option<Arc<ProxyServer>>, raw_path: &str) -> Vec<u8> {
    let proxy = match proxy {
        Some(p) => p,
        None => return json_err(503, "Logs not available in dashboard-only mode"),
    };
    let q = parse_query(raw_path);
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200);
    let server = q.get("server").map(|s| s.as_str()).filter(|s| !s.is_empty());
    let level = q.get("level").map(|s| s.as_str()).filter(|s| !s.is_empty());
    let since_id = q.get("since").and_then(|s| s.parse::<u64>().ok());

    let store = proxy.log_store();
    // recent() returns newest-first; UI usually wants chronological so we reverse here.
    let mut entries = store.recent(limit, server, level, since_id).await;
    entries.reverse();
    let total = store.len().await;

    json_ok(json!({
        "entries": entries,
        "total": total,
        "filters": {
            "server": server,
            "level": level,
            "limit": limit,
            "since_id": since_id,
        }
    }))
}

fn handle_update_settings(body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let mut config = read_config();
    if let Some(existing) = config.get_mut("settings").and_then(|v| v.as_object_mut()) {
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                existing.insert(k.clone(), v.clone());
            }
        }
    } else {
        config["settings"] = data;
    }
    if save_config(&config) {
        json_ok(json!({"ok": true, "settings": config["settings"]}))
    } else {
        json_err(500, "Failed to save config")
    }
}

fn handle_reload() -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let pid = std::process::id();
        unsafe { libc_kill(pid as i32, libc_sighup()) };
        json_ok(json!({"ok": true, "message": "SIGHUP sent, config reloading"}))
    }
    #[cfg(not(unix))]
    {
        json_ok(json!({"ok": true, "message": "Hot-reload via file watcher (5s interval)"}))
    }
}

#[cfg(unix)]
fn libc_sighup() -> i32 { 1 }

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
    kill(pid, sig);
}

fn handle_list_tokens() -> Vec<u8> {
    let store = crate::auth::load_tokens();
    let tokens: Vec<Value> = store.tokens.iter().map(|(token, entry)| {
        json!({
            "token": token,
            "name": entry.name,
            "allowed_servers": entry.allowed_servers,
            "created_at": entry.created_at,
            "last_used": entry.last_used,
        })
    }).collect();
    json_ok(json!({"tokens": tokens}))
}

fn handle_create_token(body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let name = match data.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return json_err(400, "Field 'name' required"),
    };
    let allowed_servers = data.get("allowed_servers").and_then(|v| v.as_array()).map(|arr| {
        arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()
    });
    let token = crate::auth::create_token(&name, allowed_servers);
    json_ok(json!({"ok": true, "token": token, "name": name}))
}

fn handle_revoke_token(body: &str) -> Vec<u8> {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return json_err(400, "Invalid JSON"),
    };
    let token = match data.get("token").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json_err(400, "Field 'token' required"),
    };
    if crate::auth::revoke_token(token) {
        json_ok(json!({"ok": true}))
    } else {
        json_err(404, "Token not found")
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
                        let tools_str = rest.find("... ")
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
                let ok_count = parts.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let failed = parts.get(3).and_then(|s| s.trim_end_matches(',').parse::<usize>().ok()).unwrap_or(0);
                let total = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
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
    let config = read_config();
    let key = if config.get("servers").and_then(|v| v.as_object()).is_some() { "servers" } else { "mcpServers" };
    let servers = match config.get(key).and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return json_err(404, "No servers configured"),
    };
    if !servers.contains_key(name) {
        return json_err(404, "Server not found");
    }

    let srv = &servers[name];
    let command = srv.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let args: Vec<String> = srv.get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Step 1: Check if command exists
    let cmd_check = tokio::process::Command::new("which")
        .arg(command)
        .output()
        .await;
    
    let cmd_exists = cmd_check.map(|o| o.status.success()).unwrap_or(false);
    if !cmd_exists {
        // Try to find the command in common locations
        let common_paths = [
            format!("{}/.nvm/versions/node/v25.0.0/bin/{}", dirs::home_dir().unwrap_or_default().display(), command),
            format!("{}/.nvm/versions/node/v22.22.0/bin/{}", dirs::home_dir().unwrap_or_default().display(), command),
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

    // Step 2: Try to start the server and get tools
    let mut cmd = tokio::process::Command::new(command);
    for arg in &args {
        cmd.arg(arg);
    }
    
    // Add env vars
    if let Some(env_obj) = srv.get("env").and_then(|v| v.as_object()) {
        for (k, v) in env_obj {
            if let Some(val) = v.as_str() {
                cmd.env(k, val);
            }
        }
    }
    
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
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        child.wait_with_output(),
    ).await;

    match output {
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            
            // Check for common errors
            let combined = format!("{}\n{}", stderr, stdout);
            
            if combined.contains("MODULE_NOT_FOUND") || combined.contains("Cannot find module") {
                let module = combined.lines()
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
            
            if combined.contains("API") && (combined.contains("401") || combined.contains("403") || combined.contains("unauthorized") || combined.contains("Unauthorized")) {
                return json_ok(json!({
                    "ok": false,
                    "step": "auth_error",
                    "error": "Authentication failed - API key/token may be invalid or expired",
                    "suggestion": "Update the API key/token in env vars",
                    "auto_fixable": false
                }));
            }
            
            if combined.contains("ECONNREFUSED") || combined.contains("fetch failed") || combined.contains("network") {
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
                    let gen_combined = format!("{}{}", 
                        String::from_utf8_lossy(&gen_out.stderr),
                        String::from_utf8_lossy(&gen_out.stdout)
                    );
                    let server_line = gen_combined.lines()
                        .find(|l| l.contains(name))
                        .unwrap_or("");
                    
                    if server_line.contains("FAILED") {
                        let error_part = server_line.split("FAILED:").nth(1).unwrap_or("Unknown error").trim();
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
                }))
            }
        }
        Ok(Err(e)) => {
            json_ok(json!({
                "ok": false,
                "step": "process_error",
                "error": format!("Process error: {}", e),
                "auto_fixable": false
            }))
        }
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
                    let gen_combined = format!("{}{}", 
                        String::from_utf8_lossy(&gen_out.stderr),
                        String::from_utf8_lossy(&gen_out.stdout)
                    );
                    if gen_combined.contains(&format!("{} ... ", name)) && !gen_combined.contains("FAILED") {
                        json_ok(json!({
                            "ok": true,
                            "step": "repaired",
                            "message": format!("Server '{}' repaired and cache rebuilt", name)
                        }))
                    } else {
                        let error_line = gen_combined.lines()
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
                }))
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
        ("GET", "/") => http_response(200, "OK", "text/html; charset=utf-8", DASHBOARD_HTML),
        ("GET", "/api/servers") => handle_get_servers(),
        ("POST", "/api/servers") => handle_add_server(&req.body),
        ("GET", "/api/settings") => handle_get_settings(),
        ("GET", "/api/metrics") => handle_get_metrics(proxy, sse).await,
        ("GET", "/api/logs") => handle_get_logs(proxy, &req.path).await,
        ("PUT", "/api/settings") => handle_update_settings(&req.body),
        ("POST", "/api/generate") => handle_generate().await,
        ("POST", "/api/reload") => handle_reload(),
        ("GET", "/api/tokens") => handle_list_tokens(),
        ("POST", "/api/tokens") => handle_create_token(&req.body),
        ("DELETE", "/api/tokens") => handle_revoke_token(&req.body),
        _ => {
            if path.starts_with("/api/servers/") {
                let rest = &path["/api/servers/".len()..];
                if rest.ends_with("/toggle") {
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
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

// ─── Server Entry Point ─────────────────────────────────────

/// Start dashboard only (no SSE, no proxy). For `McpHub dashboard` command.
pub async fn start_dashboard() {
    start_http(None, None, true).await;
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
    let addr = "127.0.0.1:24680";
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
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open")
            .arg(format!("http://{}", addr))
            .spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open")
            .arg(format!("http://{}", addr))
            .spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", &format!("http://{}", addr)])
            .spawn();
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
    let mut total_read = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read(&mut buf),
    ).await {
        Ok(Ok(n)) if n > 0 => n,
        _ => return, // Timeout or read error: drop connection
    };

    // Check if it's an OPTIONS request early
    if total_read >= 7 && &buf[..7] == b"OPTIONS" {
        let resp = b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Max-Age: 86400\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(resp).await;
        return;
    }

    // Find end of headers
    let mut body_offset = 0;
    for i in 0..total_read.saturating_sub(3) {
        if &buf[i..i+4] == b"\r\n\r\n" {
            body_offset = i + 4;
            break;
        } else if i < total_read.saturating_sub(1) && &buf[i..i+2] == b"\n\n" {
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

        // Read the rest of the body if needed
        let target_size = body_offset + content_length;
        if target_size > buf.len() {
            buf.resize(target_size, 0);
        }
        while total_read < target_size {
            let n = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                stream.read(&mut buf[total_read..]),
            ).await {
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

    // SSE endpoint: long-lived connection, don't close
    if path == "/sse" && req.method == "GET" {
        let auth_header = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        let bearer = auth_header.strip_prefix("Bearer ").unwrap_or("");
        if crate::auth::validate_token(bearer).is_none() {
            let resp = json_err(401, "Unauthorized");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }

        if let Some(sse_mgr) = &sse {
            sse_mgr.handle_connect(stream).await;
            return; // Connection handled, don't close
        } else {
            let resp = json_err(503, "SSE not available in dashboard-only mode");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }
    }

    // Message endpoint: process JSON-RPC via SSE session
    if path == "/message" && req.method == "POST" {
        let auth_header = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        let bearer = auth_header.strip_prefix("Bearer ").unwrap_or("");
        if crate::auth::validate_token(bearer).is_none() {
            let resp = json_err(401, "Unauthorized");
            let _ = stream.write_all(&resp).await;
            let _ = stream.shutdown().await;
            return;
        }

        let response = if let (Some(proxy_ref), Some(sse_mgr)) = (&proxy, &sse) {
            if let Some(session_id) = extract_session_id(&req.path) {
                sse_mgr.handle_message(&session_id, &req.body, proxy_ref).await
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

    // Structured MCP log stream — broadcasts entries from the in-memory LogStore.
    // Backed by `notifications/message` from child servers (see child.rs).
    if path == "/api/logs/stream" && req.method == "GET" {
        let proxy_ref = match &proxy {
            Some(p) => p.clone(),
            None => {
                let resp = json_err(503, "Logs not available in dashboard-only mode");
                let _ = stream.write_all(&resp).await;
                let _ = stream.shutdown().await;
                return;
            }
        };

        let q = parse_query(&req.path);
        let server_filter = q
            .get("server")
            .cloned()
            .filter(|s| !s.is_empty());
        let level_filter = q
            .get("level")
            .cloned()
            .filter(|s| !s.is_empty());

        let headers = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: keep-alive\r\n\
             X-Accel-Buffering: no\r\n\
             Access-Control-Allow-Origin: *\r\n\r\n";
        if stream.write_all(headers.as_bytes()).await.is_err() {
            return;
        }

        let store = proxy_ref.log_store();

        // Replay last 50 entries (chronological) so the UI is populated immediately.
        let initial = {
            let mut e = store
                .recent(
                    50,
                    server_filter.as_deref(),
                    level_filter.as_deref(),
                    None,
                )
                .await;
            e.reverse();
            e
        };
        for entry in initial {
            let payload = serde_json::to_string(&entry).unwrap_or_default();
            let event = format!("event: log\ndata: {}\n\n", payload);
            if stream.write_all(event.as_bytes()).await.is_err() {
                return;
            }
        }

        let mut rx = store.subscribe();
        let keepalive_every = std::time::Duration::from_secs(15);
        let mut last_ka = std::time::Instant::now();

        loop {
            let recv = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
            match recv {
                Ok(Ok(entry)) => {
                    if let Some(ref s) = server_filter {
                        if !entry.server.eq_ignore_ascii_case(s) {
                            continue;
                        }
                    }
                    if let Some(ref l) = level_filter {
                        if !level_passes(&entry.level, l) {
                            continue;
                        }
                    }
                    let payload = serde_json::to_string(&entry).unwrap_or_default();
                    let event = format!("event: log\ndata: {}\n\n", payload);
                    if stream.write_all(event.as_bytes()).await.is_err() {
                        return;
                    }
                    last_ka = std::time::Instant::now();
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    let event = format!(
                        "event: lag\ndata: {{\"dropped\":{}}}\n\n",
                        n
                    );
                    if stream.write_all(event.as_bytes()).await.is_err() {
                        return;
                    }
                }
                Ok(Err(_closed)) => return,
                Err(_) => {
                    if last_ka.elapsed() >= keepalive_every {
                        if stream.write_all(b": keep-alive\n\n").await.is_err() {
                            return;
                        }
                        last_ka = std::time::Instant::now();
                    }
                }
            }
        }
    }

    if path == "/api/logs-stream" && req.method == "GET" {
        let headers = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: keep-alive\r\n\
             Access-Control-Allow-Origin: *\r\n\
             \r\n";
        if stream.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        
        let log_path = config_dir().join("mcphub.log");
        if !log_path.exists() {
            let _ = stream.write_all(b"event: message\ndata: {\"error\": \"Log file not found\"}\n\n").await;
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
                        if reader.seek(SeekFrom::Start(pos)).is_err() { break; }
                    }
                    Ok(len) => {
                        pos += len as u64;
                        let line_trim = line.trim();
                        if !line_trim.is_empty() {
                            let json_msg = serde_json::json!({ "line": line_trim });
                            let event = format!("event: message\ndata: {}\n\n", json_msg.to_string());
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
