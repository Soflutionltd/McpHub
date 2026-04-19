use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub pool: usize,
    /// Per-server request timeout override (seconds). Falls back to the global default when None.
    pub request_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Discover,
    Passthrough,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Preload {
    All,
    None,
    Some(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub servers: HashMap<String, ServerConfig>,
    pub mode: Mode,
    pub preload: Preload,
    pub idle_timeout_ms: u64,
    pub preload_delay_ms: u64,
    pub health_check_interval_secs: u64,
    pub health_auto_restart: bool,
    pub health_notifications: bool,
    /// Default request timeout in seconds for child MCP calls.
    /// Per-server overrides live on `ServerConfig.request_timeout_secs`.
    pub request_timeout_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
            mode: Mode::Discover,
            preload: Preload::All,
            idle_timeout_ms: 5 * 60 * 1000,
            preload_delay_ms: 200,
            health_check_interval_secs: 30,
            health_auto_restart: true,
            health_notifications: true,
            request_timeout_secs: 120,
        }
    }
}

fn is_self(name: &str, config: &Value) -> bool {
    let lower = name.to_lowercase();
    if lower == "mcphub" || lower == "mcp-hub" || lower == "mcp-on-demand" { return true; }
    if let Some(cmd) = config.get("command").and_then(|v| v.as_str()) {
        let cmd_lower = cmd.to_lowercase();
        if cmd_lower.contains("mcphub") || cmd_lower.contains("mcp-on-demand") { return true; }
    }
    if let Some(args) = config.get("args").and_then(|v| v.as_array()) {
        if args.iter().any(|a| a.as_str().map(|s| {
            let s_lower = s.to_lowercase();
            s_lower.contains("mcphub") || s_lower.contains("mcp-on-demand")
        }).unwrap_or(false)) {
            return true;
        }
    }
    false
}

fn parse_servers(json: &Value) -> HashMap<String, ServerConfig> {
    let mut result = HashMap::new();
    let servers_obj = json.get("mcpServers").or_else(|| json.get("servers")).unwrap_or(json);
    let servers = match servers_obj.as_object() { Some(m) => m, None => return result };

    for (name, config) in servers {
        if name.starts_with('_') { continue; }
        if is_self(name, config) {
            eprintln!("[McpHub][INFO] Skipped self: {}", name);
            continue;
        }
        if config.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
            eprintln!("[McpHub][INFO] Skipped disabled: {}", name);
            continue;
        }
        if let Some(cmd) = config.get("command").and_then(|v| v.as_str()) {
            let args: Vec<String> = config.get("args").and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let env: HashMap<String, String> = config.get("env").and_then(|v| v.as_object())
                .map(|obj| obj.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                .unwrap_or_default();
            let pool = config.get("pool").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let request_timeout_secs = config
                .get("requestTimeoutSecs")
                .or_else(|| config.get("request_timeout_secs"))
                .and_then(|v| v.as_u64());
            result.insert(name.clone(), ServerConfig { command: cmd.to_string(), args, env, pool, request_timeout_secs });
        }
    }
    result
}

fn load_dedicated_config() -> Option<ProxyConfig> {
    let home = dirs::home_dir()?;
    let path = home.join(".McpHub").join("config.json");
    if !path.exists() { return None; }
    let content = fs::read_to_string(&path).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;
    let servers = parse_servers(&json);
    if servers.is_empty() { return None; }
    eprintln!("[McpHub][INFO] Loaded {} servers from {}", servers.len(), path.display());

    let mut config = ProxyConfig { servers, ..Default::default() };
    if let Some(settings) = json.get("settings") {
        if let Some(mode) = settings.get("mode").and_then(|v| v.as_str()) {
            config.mode = match mode { "passthrough" => Mode::Passthrough, _ => Mode::Discover };
        }
        if let Some(timeout) = settings.get("idleTimeout").and_then(|v| v.as_u64()) {
            config.idle_timeout_ms = timeout * 1000;
        }
        if let Some(timeout) = settings
            .get("requestTimeoutSecs")
            .or_else(|| settings.get("request_timeout_secs"))
            .and_then(|v| v.as_u64())
        {
            config.request_timeout_secs = timeout;
        }
        // Health monitor settings
        if let Some(health) = settings.get("health") {
            if let Some(interval) = health.get("checkInterval").and_then(|v| v.as_u64()) {
                config.health_check_interval_secs = interval;
            }
            if let Some(restart) = health.get("autoRestart").and_then(|v| v.as_bool()) {
                config.health_auto_restart = restart;
            }
            if let Some(notify) = health.get("notifications").and_then(|v| v.as_bool()) {
                config.health_notifications = notify;
            }
        }
    }
    Some(config)
}

fn get_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".cursor").join("mcp.json"));
        if cfg!(target_os = "macos") {
            if let Some(support) = dirs::data_dir() {
                paths.push(support.join("Claude").join("claude_desktop_config.json"));
            }
            paths.push(home.join("Library").join("Application Support").join("Claude").join("claude_desktop_config.json"));
        }
        paths.push(home.join(".codeium").join("windsurf").join("mcp_config.json"));
        paths.push(home.join(".vscode").join("mcp.json"));
    }
    paths
}

pub fn auto_detect() -> ProxyConfig {
    if let Some(config) = load_dedicated_config() {
        let mode_str = match config.mode { Mode::Discover => "discover", Mode::Passthrough => "passthrough" };
        eprintln!("[McpHub][INFO] Using dedicated config: {} servers, mode={}", config.servers.len(), mode_str);
        return apply_env_overrides(config);
    }

    let mut config = ProxyConfig::default();
    for path in &get_config_paths() {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(json) = serde_json::from_str::<Value>(&content) {
                    let servers = parse_servers(&json);
                    if !servers.is_empty() {
                        eprintln!("[McpHub][INFO] Found {} servers in {}", servers.len(), path.display());
                        config.servers.extend(servers);
                    }
                }
            }
        }
    }

    if config.servers.is_empty() {
        eprintln!("[McpHub][WARN] No MCP servers found.");
    } else {
        eprintln!("[McpHub][INFO] Total: {} servers detected", config.servers.len());
    }
    apply_env_overrides(config)
}

fn apply_env_overrides(mut config: ProxyConfig) -> ProxyConfig {
    if let Ok(mode) = std::env::var("MCP_ON_DEMAND_MODE") {
        config.mode = match mode.as_str() { "passthrough" => Mode::Passthrough, _ => Mode::Discover };
    }
    if let Ok(preload) = std::env::var("MCP_ON_DEMAND_PRELOAD") {
        config.preload = match preload.as_str() { "none" => Preload::None, _ => Preload::All };
    }
    if let Ok(timeout) = std::env::var("MCPHUB_REQUEST_TIMEOUT_SECS") {
        if let Ok(parsed) = timeout.parse::<u64>() {
            if parsed > 0 {
                config.request_timeout_secs = parsed;
            }
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_self() {
        assert!(is_self("McpHub", &json!({})));
        assert!(is_self("mcp-hub", &json!({})));
        assert!(!is_self("github", &json!({})));
        assert!(is_self("test", &json!({"command": "/path/to/mcphub"})));
        assert!(is_self("test", &json!({"command": "node", "args": ["mcphub"]})));
    }

    #[test]
    fn test_parse_servers() {
        let json = json!({
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": {
                        "GITHUB_TOKEN": "123"
                    }
                },
                "disabled_server": {
                    "command": "test",
                    "disabled": true
                },
                "McpHub": {
                    "command": "mcphub"
                }
            }
        });

        let servers = parse_servers(&json);
        
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("github"));
        assert!(!servers.contains_key("disabled_server")); // skipped disabled
        assert!(!servers.contains_key("McpHub")); // skipped self
        
        let github = &servers["github"];
        assert_eq!(github.command, "npx");
        assert_eq!(github.args.len(), 2);
        assert_eq!(github.env.get("GITHUB_TOKEN").unwrap(), "123");
    }

    #[test]
    fn test_parse_servers_no_servers() {
        let json = json!({"otherKey": "value"});
        let servers = parse_servers(&json);
        assert!(servers.is_empty());
    }
}
