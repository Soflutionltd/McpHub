/// Child process manager: spawn MCP servers, communicate over stdio, manage lifecycle.
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub use crate::config::ServerConfig;
use crate::protocol::ToolDef;
use crate::redaction::redact_text;

enum Link {
    Stdio {
        child: Child,
        stdin: tokio::process::ChildStdin,
        stdout_lines: Arc<Mutex<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>>>,
    },
    Http(Arc<crate::remote::RemoteClient>),
}

struct ChildProcess {
    link: Link,
    next_id: u64,
    tools: Vec<ToolDef>,
    last_used: Instant,
    server_name: String,
    protocol_version: String,
    request_timeout_secs: u64,
}

impl ChildProcess {
    async fn kill(&mut self) {
        match &mut self.link {
            Link::Stdio { child, .. } => {
                let _ = child.kill().await;
            }
            Link::Http(remote) => remote.close().await,
        }
    }

    /// Processus terminé ? (toujours vivant pour un serveur distant)
    fn exited(&mut self) -> Result<Option<String>, String> {
        match &mut self.link {
            Link::Stdio { child, .. } => match child.try_wait() {
                Ok(Some(status)) => Ok(Some(status.to_string())),
                Ok(None) => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            Link::Http(_) => Ok(None),
        }
    }
}

struct ServerPool {
    procs: Vec<Arc<Mutex<ChildProcess>>>,
    next_idx: AtomicUsize,
}

/// Variables système transmises à tous les serveurs.
const BASE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
    "TERM", "__CF_USER_TEXT_ENCODING", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy",
    "https_proxy", "no_proxy", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE",
];

fn base_child_env(unsandboxed: bool, inherit: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in BASE_ENV {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    // L'agent SSH permet de signer avec les clés : réservé aux serveurs non isolés.
    if unsandboxed {
        if let Ok(v) = std::env::var("SSH_AUTH_SOCK") {
            out.push(("SSH_AUTH_SOCK".into(), v));
        }
    }
    for k in inherit {
        if let Ok(v) = std::env::var(k) {
            out.push((k.clone(), v));
        }
    }
    out
}

/// Version de protocole proposée aux serveurs enfants (ils répondent avec la leur).
const CHILD_PROTOCOL_VERSION: &str = "2025-06-18";

/// Sortie d'erreur d'un serveur enfant : ~/.McpHub/logs/<serveur>.stderr.log (remise à zéro
/// au-delà de 1 Mo). Auparavant jetée, ce qui rendait les pannes de démarrage illisibles.
fn child_stderr(name: &str) -> Stdio {
    let Some(home) = dirs::home_dir() else {
        return Stdio::null();
    };
    let dir = home.join(".McpHub").join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return Stdio::null();
    }
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("{}.stderr.log", safe));
    let too_big = std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    }
}

pub struct ChildManager {
    configs: Arc<Mutex<HashMap<String, ServerConfig>>>,
    pools: Arc<Mutex<HashMap<String, Arc<ServerPool>>>>,
    idle_timeout_ms: u64,
    request_timeout_secs: u64,
}

impl ChildManager {
    pub fn new(
        configs: HashMap<String, ServerConfig>,
        idle_timeout_ms: u64,
        request_timeout_secs: u64,
    ) -> Self {
        Self {
            configs: Arc::new(Mutex::new(configs)),
            pools: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout_ms,
            request_timeout_secs: request_timeout_secs.max(1),
        }
    }

    pub async fn update_configs(&self, new_configs: HashMap<String, ServerConfig>) {
        let mut current_configs = self.configs.lock().await;

        let mut to_stop = Vec::new();
        for (name, old_cfg) in current_configs.iter() {
            if let Some(new_cfg) = new_configs.get(name) {
                if old_cfg != new_cfg {
                    to_stop.push(name.clone());
                }
            } else {
                to_stop.push(name.clone());
            }
        }

        for name in to_stop {
            self.stop_server(&name).await;
        }

        *current_configs = new_configs;
    }

    /// Nom canonique (clé de config) d'un serveur. Utilisé aussi par le proxy pour que
    /// les permissions s'appliquent exactement au serveur qui sera appelé.
    pub async fn resolve_name(&self, name: &str) -> Option<String> {
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
        let name_resolved = self
            .resolve_name(name)
            .await
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
                let delay = BACKOFF_MS
                    .get(attempt as usize - 1)
                    .copied()
                    .unwrap_or(2000);
                eprintln!(
                    "[McpHub][RETRY] {} attempt {}/{} (backoff {}ms)",
                    name,
                    attempt + 1,
                    MAX_RETRIES,
                    delay
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }

            match self.try_start_pool(name).await {
                Ok(tools) => return Ok(tools),
                Err(e) => {
                    last_error = e;
                    if attempt < MAX_RETRIES - 1 {
                        eprintln!(
                            "[McpHub][WARN] {} failed: {} — retrying...",
                            name, last_error
                        );
                    }
                }
            }
        }

        Err(format!("{} (after {} attempts)", last_error, MAX_RETRIES))
    }

    async fn try_start_pool(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let config = {
            let configs = self.configs.lock().await;
            configs
                .get(name)
                .ok_or_else(|| format!("Unknown server: {}", name))?
                .clone()
        };

        let pool_size = config.pool.max(1);
        let mut procs = Vec::new();
        let mut first_tools = Vec::new();

        for i in 0..pool_size {
            let start = Instant::now();
            if pool_size > 1 {
                eprintln!(
                    "[McpHub][INFO] Starting server: {} (instance {}/{})",
                    name,
                    i + 1,
                    pool_size
                );
            } else {
                eprintln!("[McpHub][INFO] Starting server: {}", name);
            }

            let link = if let Some(url) = &config.url {
                if i == 0 {
                    eprintln!("[McpHub][INFO] {} : serveur distant {}", name, url);
                }
                Link::Http(Arc::new(crate::remote::RemoteClient::new(
                    name,
                    url,
                    &config.headers,
                    config.oauth,
                )))
            } else {
                let (program, program_args) = crate::sandbox::wrap(
                    name,
                    config.sandbox.as_ref(),
                    &config.command,
                    &config.args,
                    &config.env,
                );
                if i == 0 && config.sandbox.is_some() {
                    eprintln!("[McpHub][SANDBOX] {} : isole ({:?})", name, config.sandbox.as_ref().map(|s| s.mode));
                }
                let mut cmd = Command::new(&program);
                cmd.args(&program_args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(child_stderr(name));

                // Environnement minimal : les secrets chargés dans le démon (Trousseau, plist) ne
                // doivent jamais fuiter vers TOUS les serveurs. Chacun ne reçoit que la base système,
                // ses propres variables (`env`) et celles qu'il demande nommément (`inheritEnv`).
                cmd.env_clear();
                for (k, v) in base_child_env(config.sandbox.is_none(), &config.inherit_env) {
                    cmd.env(k, v);
                }
                for (k, v) in &config.env {
                    cmd.env(k, v);
                }

                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("Failed to spawn {}: {}", name, e))?;
                let stdin = child.stdin.take().ok_or("No stdin")?;
                let stdout = child.stdout.take().ok_or("No stdout")?;

                let reader = BufReader::new(stdout);
                let lines = Arc::new(Mutex::new(reader.lines()));
                Link::Stdio {
                    child,
                    stdin,
                    stdout_lines: lines,
                }
            };

            let mut proc = ChildProcess {
                link,
                next_id: 1,
                tools: Vec::new(),
                last_used: Instant::now(),
                server_name: name.to_string(),
                protocol_version: "2024-11-05".to_string(),
                request_timeout_secs: config
                    .request_timeout_secs
                    .unwrap_or(self.request_timeout_secs)
                    .max(1),
            };

            let init_result = send_request(
                &mut proc,
                "initialize",
                serde_json::json!({
                    "protocolVersion": CHILD_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "McpHub", "version": "4.0.0" }
                }),
            )
            .await?;

            if let Some(pv) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
                proc.protocol_version = pv.to_string();
                if i == 0 {
                    eprintln!(
                        "[McpHub][INFO] Server '{}' negotiated protocol: {}",
                        name, pv
                    );
                }
            }

            send_notification(
                &mut proc,
                "notifications/initialized",
                serde_json::json!({}),
            )
            .await?;
            // tools/list est paginé (nextCursor) : on lit toutes les pages.
            let mut tools: Vec<ToolDef> = Vec::new();
            let mut cursor: Option<String> = None;
            for _page in 0..50 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({ "cursor": c }),
                    None => serde_json::json!({}),
                };
                let tools_result = send_request(&mut proc, "tools/list", params).await?;
                if let Some(arr) = tools_result.get("tools").and_then(|v| v.as_array()) {
                    for t in arr {
                        // Un outil mal formé n'empêche plus de lire les autres.
                        match serde_json::from_value::<ToolDef>(t.clone()) {
                            Ok(def) => tools.push(def),
                            Err(e) => eprintln!(
                                "[McpHub][WARN] {} : outil ignore (definition invalide : {})",
                                name, e
                            ),
                        }
                    }
                }
                cursor = tools_result
                    .get("nextCursor")
                    .and_then(|v| v.as_str())
                    .filter(|c| !c.is_empty())
                    .map(String::from);
                if cursor.is_none() {
                    break;
                }
            }

            // Épinglage / détection d'empoisonnement sur la définition réellement annoncée,
            // pour chaque instance du pool.
            crate::toolguard::review(name, &tools);
            if i == 0 {
                let elapsed = start.elapsed();
                eprintln!(
                    "[McpHub][INFO] Server '{}' ready: {} tools in {:.0}ms",
                    name,
                    tools.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
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
        let resolved = self
            .resolve_name(server_name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        if !self.is_running(server_name).await {
            return Err(format!("Server not running: {}", server_name));
        }

        let pool = {
            let pools = self.pools.lock().await;
            pools
                .get(server_name)
                .cloned()
                .ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
        let result = {
            let mut proc = pool.procs[idx].lock().await;
            proc.last_used = Instant::now();
            send_request(&mut proc, method, arguments.clone()).await
        };

        match result {
            Err(e) if is_connection_error(&e) => {
                eprintln!(
                    "[McpHub][WARN] Connection error on '{}': {}. Retrying...",
                    server_name, e
                );
                self.restart_server(server_name).await?;

                let pool = {
                    let pools = self.pools.lock().await;
                    pools
                        .get(server_name)
                        .cloned()
                        .ok_or_else(|| format!("Server not running: {}", server_name))?
                };

                let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
                let mut proc = pool.procs[idx].lock().await;
                proc.last_used = Instant::now();
                send_request(&mut proc, method, arguments).await
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
        let resolved = self
            .resolve_name(server_name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        if !self.is_running(server_name).await {
            self.start_server(server_name).await?;
        }

        let pool = {
            let pools = self.pools.lock().await;
            pools
                .get(server_name)
                .cloned()
                .ok_or_else(|| format!("Server not running: {}", server_name))?
        };

        let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
        let result = {
            let mut proc = pool.procs[idx].lock().await;
            proc.last_used = Instant::now();
            send_request(
                &mut proc,
                "tools/call",
                serde_json::json!({ "name": tool_name, "arguments": arguments.clone() }),
            )
            .await
        };

        match result {
            Err(e) if is_connection_error(&e) => {
                eprintln!(
                    "[McpHub][WARN] Connection error on '{}': {}. Retrying...",
                    server_name, e
                );
                self.restart_server(server_name).await?;
                // Le serveur relancé a pu changer ses définitions : re-contrôle avant l'envoi.
                if let Some(reason) = crate::toolguard::quarantined(server_name, tool_name) {
                    return Err(format!(
                        "Bloque par McpHub apres redemarrage de {} : outil {} en quarantaine ({})",
                        server_name, tool_name, reason
                    ));
                }

                let pool = {
                    let pools = self.pools.lock().await;
                    pools
                        .get(server_name)
                        .cloned()
                        .ok_or_else(|| format!("Server not running: {}", server_name))?
                };

                let idx = pool.next_idx.fetch_add(1, Ordering::Relaxed) % pool.procs.len();
                let mut proc = pool.procs[idx].lock().await;
                proc.last_used = Instant::now();
                send_request(
                    &mut proc,
                    "tools/call",
                    serde_json::json!({ "name": tool_name, "arguments": arguments }),
                )
                .await
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
                proc.kill().await;
            }
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    pub async fn stop_all(&self) {
        let mut pools = self.pools.lock().await;
        for (name, pool) in pools.drain() {
            for proc_arc in &pool.procs {
                let mut proc = proc_arc.lock().await;
                proc.kill().await;
            }
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    pub async fn server_names(&self) -> Vec<String> {
        let configs = self.configs.lock().await;
        configs.keys().cloned().collect()
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
            pools
                .get(server_name)
                .cloned()
                .ok_or_else(|| format!("Server not running: {}", server_name))?
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
                    proc.kill().await;
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

                match proc.exited() {
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
                )
                .await;

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
                        proc.kill().await;
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
                    proc.kill().await;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let tools = self.start_server(name).await?;
        Ok(tools.len())
    }
}

fn is_connection_error(e: &str) -> bool {
    e.contains("Write error")
        || e.contains("Flush error")
        || e.contains("Read error")
        || e.contains("Server closed connection")
}

async fn send_request(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let timeout_secs = proc.request_timeout_secs.max(1);
    let timeout = std::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, send_request_inner(proc, method, params)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Timeout: server did not respond within {}s",
            timeout_secs
        )),
    }
}

async fn send_request_inner(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (stdin, stdout_lines) = match &mut proc.link {
        Link::Http(remote) => {
            let remote = remote.clone();
            return remote.request(method, params).await;
        }
        Link::Stdio { stdin, stdout_lines, .. } => (stdin, stdout_lines.clone()),
    };
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

    stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| format!("Write error: {}", e))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;

    let mut lines = stdout_lines.lock().await;
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
                        if let Some(level) = params.get("level").and_then(|v| v.as_str()) {
                            if let Some(data) = params.get("data").and_then(|v| v.as_str()) {
                                eprintln!(
                                    "[McpHub][{}][{}] {}",
                                    proc.server_name,
                                    level.to_uppercase(),
                                    redact_text(data)
                                );
                            }
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
                return Ok(parsed
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        }
    }
}

async fn send_notification(
    proc: &mut ChildProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<(), String> {
    let stdin = match &mut proc.link {
        Link::Http(remote) => {
            let remote = remote.clone();
            return remote.notify(method, params).await;
        }
        Link::Stdio { stdin, .. } => stdin,
    };
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });

    let mut msg = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
    msg.push('\n');

    stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| format!("Write error: {}", e))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;

    Ok(())
}
