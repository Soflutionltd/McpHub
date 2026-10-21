use crate::config::ServerConfig;
use crate::protocol::ToolDef;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
pub struct SchemaCache {
    pub version: String,
    pub servers: HashMap<String, Vec<ToolDef>>,
    #[serde(default)]
    pub errors: HashMap<String, String>,
    #[serde(default)]
    pub server_hashes: HashMap<String, String>,
}

pub fn cache_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".McpHub").join("schema-cache.json"))
}

pub fn load_cache() -> Option<SchemaCache> {
    let path = cache_path()?;
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    let cache: SchemaCache = serde_json::from_str(&content).ok()?;
    let total_tools: usize = cache.servers.values().map(|v| v.len()).sum();
    eprintln!(
        "[McpHub][INFO] Loaded cache: {} servers, {} tools",
        cache.servers.len(),
        total_tools
    );
    Some(cache)
}

pub fn save_cache_with_errors(
    servers: &HashMap<String, Vec<ToolDef>>,
    errors: &HashMap<String, String>,
) {
    save_cache_with_hashes(servers, errors, &HashMap::new());
}

pub fn save_cache_with_hashes(
    servers: &HashMap<String, Vec<ToolDef>>,
    errors: &HashMap<String, String>,
    server_hashes: &HashMap<String, String>,
) {
    let cache = SchemaCache {
        version: env!("CARGO_PKG_VERSION").to_string(),
        servers: servers.clone(),
        errors: errors.clone(),
        server_hashes: server_hashes.clone(),
    };
    save_cache_state(&cache);
}

pub fn save_cache_state(cache: &SchemaCache) {
    if let Some(path) = cache_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(cache) {
            let _ = fs::write(&path, json);
            let total_tools: usize = cache.servers.values().map(|v| v.len()).sum();
            eprintln!(
                "[McpHub][INFO] Saved cache: {} servers, {} tools, {} errors",
                cache.servers.len(),
                total_tools,
                cache.errors.len()
            );
        }
    }
}

pub fn server_config_hash(config: &ServerConfig) -> String {
    let mut hasher = DefaultHasher::new();
    config.command.hash(&mut hasher);
    config.args.hash(&mut hasher);
    config.pool.hash(&mut hasher);
    config.request_timeout_secs.hash(&mut hasher);
    format!("{:?}", config.sandbox).hash(&mut hasher);
    config.inherit_env.hash(&mut hasher);
    config.url.hash(&mut hasher);
    config.oauth.hash(&mut hasher);
    let mut header_pairs: Vec<(&String, &String)> = config.headers.iter().collect();
    header_pairs.sort();
    header_pairs.hash(&mut hasher);

    let mut env_pairs: Vec<(&String, &String)> = config.env.iter().collect();
    env_pairs.sort_by(|left, right| left.0.cmp(right.0));
    for (key, value) in env_pairs {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

pub fn server_config_hashes(configs: &HashMap<String, ServerConfig>) -> HashMap<String, String> {
    configs
        .iter()
        .map(|(name, config)| (name.clone(), server_config_hash(config)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_server_config() -> ServerConfig {
        ServerConfig {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "demo".to_string()],
            env: HashMap::from([
                ("API_KEY".to_string(), "secret".to_string()),
                ("PATH".to_string(), "/usr/bin".to_string()),
            ]),
            pool: 1,
            request_timeout_secs: None,
            sandbox: None,
            inherit_env: Vec::new(),
            url: None,
            headers: HashMap::new(),
            oauth: false,
        }
    }

    #[test]
    fn test_server_config_hash_is_stable_for_env_order() {
        let mut left = mock_server_config();
        let mut right = mock_server_config();
        right.env = HashMap::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("API_KEY".to_string(), "secret".to_string()),
        ]);

        assert_eq!(server_config_hash(&left), server_config_hash(&right));

        left.args.push("--extra".to_string());
        assert_ne!(server_config_hash(&left), server_config_hash(&right));
    }

    #[test]
    fn test_server_config_hash_changes_when_timeout_changes() {
        let left = mock_server_config();
        let mut right = mock_server_config();
        right.request_timeout_secs = Some(240);

        assert_ne!(server_config_hash(&left), server_config_hash(&right));
    }
}
