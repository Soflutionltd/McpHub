/// Child process manager: spawn MCP servers, communicate over stdio, manage lifecycle.
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub use crate::config::ServerConfig;
use crate::log_store::LogStore;
use crate::protocol::ToolDef;

fn guess_stderr_level(line: &str) -> &'static str {
    let l = line.to_ascii_lowercase();
    if l.contains("error") || l.contains("fatal") || l.contains("panic") || l.contains("traceback") {
        "error"
    } else if l.contains("warn") {
        "warning"
    } else if l.contains("debug") {
        "debug"
    } else {
        "info"
    }
}

struct ChildProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: Arc<Mutex<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>>>,
    next_id: u64,
    tools: Vec<ToolDef>,
    last_used: Instant,
    server_name: String,
    #[allow(dead_code)]
    protocol_version: String,
    log_store: Option<Arc<LogStore>>,
}

struct ServerPool {
    procs: Vec<Arc<Mutex<ChildProcess>>>,
    next_idx: AtomicUsize,
}

pub struct ChildManager {
    configs: Arc<Mutex<HashMap<String, ServerConfig>>>,
    pools: Arc<Mutex<HashMap<String, Arc<ServerPool>>>>,
    idle_timeout_ms: u64,
    default_request_timeout_secs: std::sync::atomic::AtomicU64,
    log_store: Arc<LogStore>,
}

/// Result of a config diff during hot-reload.
#[derive(Debug, Default, Clone)]
pub struct ConfigDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

impl ChildManager {
    pub fn new(configs: HashMap<String, ServerConfig>, idle_timeout_ms: u64) -> Self {
        Self::with_timeout(configs, idle_timeout_ms, 120)
    }

    pub fn with_timeout(
        configs: HashMap<String, ServerConfig>,
        idle_timeout_ms: u64,
        default_request_timeout_secs: u64,
    ) -> Self {
        Self::with_full_options(
            configs,
            idle_timeout_ms,
            default_request_timeout_secs,
            Arc::new(LogStore::new()),
        )
    }

    pub fn with_full_options(
        configs: HashMap<String, ServerConfig>,
        idle_timeout_ms: u64,
        default_request_timeout_secs: u64,
        log_store: Arc<LogStore>,
    ) -> Self {
        Self {
            configs: Arc::new(Mutex::new(configs)),
            pools: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout_ms,
            default_request_timeout_secs: std::sync::atomic::AtomicU64::new(
                default_request_timeout_secs.max(1),
            ),
            log_store,
        }
    }

    pub fn log_store(&self) -> Arc<LogStore> {
        self.log_store.clone()
    }

    pub fn set_default_timeout(&self, secs: u64) {
        self.default_request_timeout_secs
            .store(secs.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    fn default_timeout(&self) -> u64 {
        self.default_request_timeout_secs
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn timeout_for(&self, server_name: &str) -> u64 {
        let configs = self.configs.lock().await;
        configs
            .get(server_name)
            .and_then(|c| c.request_timeout_secs)
            .unwrap_or_else(|| self.default_timeout())
    }

    /// Update configs and return the diff (added / removed / changed).
    /// Stopped servers will be lazily restarted on next call.
    pub async fn update_configs(&self, new_configs: HashMap<String, ServerConfig>) -> ConfigDiff {
        let mut current_configs = self.configs.lock().await;
        let mut diff = ConfigDiff::default();

        for (name, old_cfg) in current_configs.iter() {
            if let Some(new_cfg) = new_configs.get(name) {
                if old_cfg != new_cfg {
                    diff.changed.push(name.clone());
                }
            } else {
                diff.removed.push(name.clone());
            }
        }
        for name in new_configs.keys() {
            if !current_configs.contains_key(name) {
                diff.added.push(name.clone());
            }
        }

        let to_stop: Vec<String> = diff
            .removed
            .iter()
            .chain(diff.changed.iter())
            .cloned()
            .collect();

        // Replace configs first so stop_server still has access if needed,
        // then drop the lock before stopping (stop_server takes its own locks).
        *current_configs = new_configs;
        drop(current_configs);

        for name in to_stop {
            self.stop_server(&name).await;
        }

        diff
    }

    async fn resolve_name(&self, name: &str) -> Option<String> {
        let configs = self.configs.lock().await;
        if configs.contains_key(name) {
            return Some(name.to_string());
        }
        let lower = name.to_lowercase();
        for key in configs.keys() {
            if key.to_lowercase() == lower {
                return Some(key.clone());
            }
        }
        let normalized = lower.replace(['-', '_'], "");
        for key in configs.keys() {
            let key_normalized = key.to_lowercase().replace(['-', '_'], "");
            if key_normalized == normalized {
                return Some(key.clone());
            }
        }
        None
    }

    pub async fn start_server(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let name_resolved = self.resolve_name(name).await
            .ok_or_else(|| format!("Unknown server: {}", name))?;
        let name = name_resolved.as_str();

        {
            let pools = self.pools.lock().await;
            if let Some(pool) = pools.get(name) {
                let mut proc = pool.procs[0].lock().await;
                proc.last_used = Instant::now();
                return Ok(proc.tools.clone());
            }
        }

        const MAX_RETRIES: u32 = 3;
        const BACKOFF_MS: [u64; 3] = [500, 1000, 2000];
        let mut last_error = String::new();

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = BACKOFF_MS.get(attempt as usize - 1).copied().unwrap_or(2000);
                eprintln!("[McpHub][RETRY] {} attempt {}/{} (backoff {}ms)", name, attempt + 1, MAX_RETRIES, delay);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }

            match self.try_start_pool(name).await {
                Ok(tools) => return Ok(tools),
                Err(e) => {
                    last_error = e;
                    if attempt < MAX_RETRIES - 1 {
                        eprintln!("[McpHub][WARN] {} failed: {} — retrying...", name, last_error);
                    }
                }
            }
        }

        Err(format!("{} (after {} attempts)", last_error, MAX_RETRIES))
    }

    async fn try_start_pool(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let config = {
            let configs = self.configs.lock().await;
            configs.get(name).ok_or_else(|| format!("Unknown server: {}", name))?.clone()
        };

        let pool_size = config.pool.max(1);
        let mut procs = Vec::new();
        let mut first_tools = Vec::new();

        for i in 0..pool_size {
            let start = Instant::now();
            if pool_size > 1 {
                eprintln!("[McpHub][INFO] Starting server: {} (instance {}/{})", name, i + 1, pool_size);
            } else {
                eprintln!("[McpHub][INFO] Starting server: {}", name);
            }

            let mut cmd = Command::new(&config.command);
            cmd.args(&config.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            for (k, v) in &config.env {
                cmd.env(k, v);
            }

            let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn {}: {}", name, e))?;
            let stdin = child.stdin.take().ok_or("No stdin")?;
            let stdout = child.stdout.take().ok_or("No stdout")?;
            // Capture stderr lines into the LogStore. Each child gets its own task that lives
            // as long as the process. The store is bounded so memory stays in check.
            if let Some(stderr) = child.stderr.take() {
                let store = self.log_store.clone();
                let server_name = name.to_string();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let reader = BufReader::new(stderr);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let level = guess_stderr_level(trimmed);
                        store
                            .push(&server_name, level, Some("stderr".into()), trimmed.to_string())
                            .await;
                    }
                });
            }

            let reader = BufReader::new(stdout);
            let lines = Arc::new(Mutex::new(reader.lines()));

            let mut proc = ChildProcess {
                child,
                stdin,
                stdout_lines: lines,
                next_id: 1,
                tools: Vec::new(),
                last_used: Instant::now(),
                server_name: name.to_string(),
                protocol_version: "2024-11-05".to_string(),
                log_store: Some(self.log_store.clone()),
            };

            let init_result = send_request(
                &mut proc,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "McpHub", "version": "4.0.0" }
                }),
            )
            .await?;

            if let Some(pv) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
                proc.protocol_version = pv.to_string();
                if i == 0 {
                    eprintln!("[McpHub][INFO] Server '{}' negotiated protocol: {}", name, pv);
                }
            }

            send_notification(&mut proc, "notifications/initialized", serde_json::json!({})).await?;
            let tools_result = send_request(&mut proc, "tools/list", serde_json::json!({})).await?;
            let tools: Vec<ToolDef> = tools_result
                .get("tools")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            if i == 0 {
                let elapsed = start.elapsed();
                eprintln!("[McpHub][INFO] Server '{}' ready: {} tools in {:.0}ms", name, tools.len(), elapsed.as_secs_f64() * 1000.0);
                first_tools = tools.clone();
            }

            proc.tools = tools;
            procs.push(Arc::new(Mutex::new(proc)));
        }

        let pool = Arc::new(ServerPool {
            procs,
            next_idx: AtomicUsize::new(0),
        });

        let mut pools = self.pools.lock().await;
        pools.insert(name.to_string(), pool);

        Ok(first_tools)
    }

    pub async fn call_method(
        &self,
        server_name: &str,
        method: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resolved = self.resolve_name(server_name).await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        if !self.is_running(server_name).await {
            return Err(format!("Server not running: {}", server_name));
        }

        let timeout_secs = self.timeout_for(server_name).await;

        let pool = {
            let pools = self.pools.lock().await;
            pools.get(server_name).cloned().ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
        let result = {
            let mut proc = pool.procs[idx].lock().await;
            proc.last_used = Instant::now();
            send_request_with_timeout(&mut proc, method, arguments.clone(), timeout_secs).await
        };

        match result {
            Err(e) if is_connection_error(&e) => {
                eprintln!("[McpHub][WARN] Connection error on '{}': {}. Retrying...", server_name, e);
                self.restart_server(server_name).await?;

                let pool = {
                    let pools = self.pools.lock().await;
                    pools.get(server_name).cloned().ok_or_else(|| format!("Server not running: {}", server_name))?
                };

                let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
                let mut proc = pool.procs[idx].lock().await;
                proc.last_used = Instant::now();
                send_request_with_timeout(&mut proc, method, arguments, timeout_secs).await
            }
            other => other,
        }
    }

    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resolved = self.resolve_name(server_name).await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        if !self.is_running(server_name).await {
            self.start_server(server_name).await?;
        }

        let timeout_secs = self.timeout_for(server_name).await;

        let pool = {
            let pools = self.pools.lock().await;
            pools.get(server_name).cloned().ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
        let result = {
            let mut proc = pool.procs[idx].lock().await;
            proc.last_used = Instant::now();
            send_request_with_timeout(
                &mut proc,
                "tools/call",
                serde_json::json!({ "name": tool_name, "arguments": arguments.clone() }),
                timeout_secs,
            ).await
        };

        match result {
            Err(e) if is_connection_error(&e) => {
                eprintln!("[McpHub][WARN] Connection error on '{}': {}. Retrying...", server_name, e);
                self.restart_server(server_name).await?;

                let pool = {
                    let pools = self.pools.lock().await;
                    pools.get(server_name).cloned().ok_or_else(|| format!("Server not running: {}", server_name))?
                };

                let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
                let mut proc = pool.procs[idx].lock().await;
                proc.last_used = Instant::now();
                send_request_with_timeout(
                    &mut proc,
                    "tools/call",
                    serde_json::json!({ "name": tool_name, "arguments": arguments }),
                    timeout_secs,
                ).await
            }
            other => other,
        }
    }

    pub async fn is_running(&self, name: &str) -> bool {
        let pools = self.pools.lock().await;
        pools.contains_key(name)
    }

    #[allow(dead_code)]
    pub async fn stop_server(&self, name: &str) {
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.remove(name) {
            for proc_arc in &pool.procs {
                let mut proc = proc_arc.lock().await;
                let _ = proc.child.kill().await;
            }
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    pub async fn stop_all(&self) {
        let mut pools = self.pools.lock().await;
        for (name, pool) in pools.drain() {
            for proc_arc in &pool.procs {
                let mut proc = proc_arc.lock().await;
                let _ = proc.child.kill().await;
            }
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    pub async fn server_names(&self) -> Vec<String> {
        let configs = self.configs.lock().await;
        configs.keys().cloned().collect()
    }

    /// Fetch the tool list from a server, starting it if necessary.
    /// Used by the discover fallback when the search index is cold.
    pub async fn list_tools(&self, server_name: &str) -> Result<Vec<ToolDef>, String> {
        let resolved = self.resolve_name(server_name).await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        if !self.is_running(server_name).await {
            self.start_server(server_name).await?;
        }

        let pool = {
            let pools = self.pools.lock().await;
            pools.get(server_name).cloned()
                .ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        let proc = pool.procs[0].lock().await;
        Ok(proc.tools.clone())
    }

    pub async fn request_all_running(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Vec<(String, Result<serde_json::Value, String>)> {
        let running_servers: Vec<String> = {
            let pools = self.pools.lock().await;
            pools.keys().cloned().collect()
        };

        let mut results = Vec::new();
        for name in running_servers {
            let pool_opt = {
                let pools = self.pools.lock().await;
                pools.get(&name).cloned()
            };
            if let Some(pool) = pool_opt {
                let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
                let mut proc = pool.procs[idx].lock().await;
                proc.last_used = Instant::now();
                let res = send_request(&mut proc, method, params.clone()).await;
                results.push((name, res));
            } else {
                results.push((name, Err("Server stopped".into())));
            }
        }
        
        results
    }

    pub async fn forward_notification(
        &self,
        server_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), String> {
        let pool = {
            let pools = self.pools.lock().await;
            pools.get(server_name).cloned().ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        // Forward to all instances in the pool to ensure it hits the right one
        for proc_arc in &pool.procs {
            let mut proc = proc_arc.lock().await;
            proc.last_used = Instant::now();
            let _ = send_notification(&mut proc, method, params.clone()).await;
        }

        Ok(())
    }

    pub async fn reap_idle(&self) {
        let timeout = std::time::Duration::from_millis(self.idle_timeout_ms);
        let mut pools = self.pools.lock().await;

        let mut idle_servers = Vec::new();
        for (name, pool) in pools.iter() {
            let mut all_idle = true;
            for proc_arc in &pool.procs {
                let proc = proc_arc.lock().await;
                if proc.last_used.elapsed() <= timeout {
                    all_idle = false;
                    break;
                }
            }
            if all_idle {
                idle_servers.push(name.clone());
            }
        }

        for name in idle_servers {
            if let Some(pool) = pools.remove(&name) {
                for proc_arc in &pool.procs {
                    let mut proc = proc_arc.lock().await;
                    let _ = proc.child.kill().await;
                }
                eprintln!("[McpHub][INFO] Idle-stopped server: {}", name);
            }
        }
    }

    pub async fn health_check(&self) -> Vec<(String, String)> {
        let mut dead_servers: Vec<(String, String)> = Vec::new();
        let mut pools = self.pools.lock().await;

        let names: Vec<String> = pools.keys().cloned().collect();
        for name in names {
            let pool = match pools.get(&name) {
                Some(p) => p.clone(),
                None => continue,
            };

            let mut pool_dead = false;
            let mut reason = String::new();

            for proc_arc in &pool.procs {
                let mut proc = proc_arc.lock().await;
                
                match proc.child.try_wait() {
                    Ok(Some(status)) => {
                        pool_dead = true;
                        reason = format!("Process exited: {}", status);
                        break;
                    }
                    Ok(None) => {} 
                    Err(e) => {
                        pool_dead = true;
                        reason = format!("Process check failed: {}", e);
                        break;
                    }
                }

                let ping_timeout = std::time::Duration::from_secs(5);
                let ping_result = tokio::time::timeout(
                    ping_timeout,
                    send_request_inner(&mut proc, "ping", serde_json::json!({})),
                ).await;

                match ping_result {
                    Ok(Ok(_)) => {} 
                    Ok(Err(e)) => {
                        pool_dead = true;
                        reason = format!("Ping error: {}", e);
                        break;
                    }
                    Err(_) => {
                        pool_dead = true;
                        reason = "Ping timeout (5s)".to_string();
                        break;
                    }
                }
            }

            if pool_dead {
                dead_servers.push((name.clone(), reason));
                if let Some(pool) = pools.remove(&name) {
                    for proc_arc in &pool.procs {
                        let mut proc = proc_arc.lock().await;
                        let _ = proc.child.kill().await;
                    }
                }
            }
        }

        dead_servers
    }

    pub async fn restart_server(&self, name: &str) -> Result<usize, String> {
        {
            let mut pools = self.pools.lock().await;
            if let Some(pool) = pools.remove(name) {
                for proc_arc in &pool.procs {
                    let mut proc = proc_arc.lock().await;
                    let _ = proc.child.kill().await;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let tools = self.start_server(name).await?;
        Ok(tools.len())
    }
}

fn is_connection_error(e: &str) -> bool {
    e.contains("Write error") || e.contains("Flush error") || e.contains("Read error") || e.contains("Server closed connection")
}

/// Default timeout used by the legacy `send_request` helper (initialize, tools/list, etc).
/// The configurable per-server timeout is applied via `send_request_with_timeout` for
/// tools/call and call_method (the long-running paths).
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;

async fn send_request(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    send_request_with_timeout(proc, method, params, DEFAULT_REQUEST_TIMEOUT_SECS).await
}

async fn send_request_with_timeout(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
    timeout_secs: u64,
) -> Result<serde_json::Value, String> {
    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    match tokio::time::timeout(timeout, send_request_inner(proc, method, params)).await {
        Ok(result) => result,
        Err(_) => Err(format!("Timeout: server did not respond within {}s", timeout_secs)),
    }
}

async fn send_request_inner(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = proc.next_id;
    proc.next_id += 1;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let mut msg = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    msg.push('\n');

    proc.stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| format!("Write error: {}", e))?;
    proc.stdin
        .flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;

    let mut lines = proc.stdout_lines.lock().await;
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| format!("Read error: {}", e))?
            .ok_or("Server closed connection")?;

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed.get("id").is_none() {
            if let Some(method) = parsed.get("method").and_then(|v| v.as_str()) {
                if method == "notifications/message" {
                    if let Some(params) = parsed.get("params") {
                        let level = params
                            .get("level")
                            .and_then(|v| v.as_str())
                            .unwrap_or("info")
                            .to_string();
                        let logger = params
                            .get("logger")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let message = match params.get("data") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };

                        eprintln!(
                            "[McpHub][{}][{}] {}",
                            proc.server_name,
                            level.to_uppercase(),
                            message
                        );

                        if let Some(store) = &proc.log_store {
                            store
                                .push(&proc.server_name, &level, logger, message)
                                .await;
                        }
                    }
                }
            }
            continue;
        }

        if let Some(resp_id) = parsed.get("id") {
            if resp_id.as_u64() == Some(id) {
                if let Some(error) = parsed.get("error") {
                    return Err(format!("MCP error: {}", error));
                }
                return Ok(parsed.get("result").cloned().unwrap_or(serde_json::Value::Null));
            }
        }
    }
}

async fn send_notification(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<(), String> {
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });

    let mut msg = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
    msg.push('\n');

    proc.stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| format!("Write error: {}", e))?;
    proc.stdin
        .flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;

    Ok(())
}