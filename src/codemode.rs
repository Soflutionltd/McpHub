//! Code mode : l'agent envoie un petit programme JavaScript qui enchaîne plusieurs appels
//! d'outils (boucles, filtres, agrégations) et ne reçoit que le résultat final.
//!
//! Pourquoi : au lieu de 20 allers-retours outil par outil (chacun recopie de gros résultats
//! dans le contexte de l'IA), un seul appel `mcphub_run` fait le travail et renvoie un résumé.
//! C'est ce que font Cloudflare, Anthropic et Uber pour réduire les tokens.
//!
//! Sécurité : le code tourne dans un processus Node séparé, lancé avec `--permission` (aucun
//! accès fichiers, réseau, sous-processus, workers) et, sur macOS, sous `sandbox-exec` sans
//! réseau ni écriture. Son seul accès au monde passe par `mcp.call` / `mcp.search`, relayés par
//! McpHub et donc soumis aux permissions, à la quarantaine, aux profils et au journal d'audit.

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;

pub const RUNNER: &str = include_str!("codemode_runner.mjs");

const SANDBOX_PROFILE: &str = "(version 1)(allow default)(deny network*)(deny file-write*)(allow file-write-data (literal \"/dev/null\"))";

#[derive(Debug, Clone, PartialEq)]
pub struct CodeModeConfig {
    pub enabled: bool,
    pub node: Option<String>,
    pub timeout_secs: u64,
    pub max_calls: usize,
}

impl Default for CodeModeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            node: None,
            timeout_secs: 60,
            max_calls: 100,
        }
    }
}

pub fn parse(settings: Option<&Value>) -> CodeModeConfig {
    let mut c = CodeModeConfig::default();
    let Some(v) = settings.and_then(|s| s.get("codeMode")) else {
        return c;
    };
    if let Some(b) = v.as_bool() {
        c.enabled = b;
        return c;
    }
    if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
        c.enabled = b;
    }
    if let Some(n) = v.get("node").and_then(|x| x.as_str()) {
        c.node = Some(n.to_string());
    }
    if let Some(t) = v.get("timeoutSecs").and_then(|x| x.as_u64()) {
        c.timeout_secs = t.clamp(5, 300);
    }
    if let Some(m) = v.get("maxCalls").and_then(|x| x.as_u64()) {
        c.max_calls = (m as usize).clamp(1, 1000);
    }
    c
}

fn node_major(path: &PathBuf) -> u32 {
    // ~/.nvm/versions/node/v25.0.0/bin/node -> 25
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .find_map(|s| s.strip_prefix('v').and_then(|r| r.split('.').next()?.parse().ok()))
        .unwrap_or(0)
}

/// Node récent (>= 20 pour `--permission`), du réglage, sinon nvm (plus haute version), sinon
/// Homebrew / système.
pub fn find_node(cfg: &CodeModeConfig) -> Option<PathBuf> {
    if let Some(n) = &cfg.node {
        let p = PathBuf::from(n);
        return p.exists().then_some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            for e in rd.flatten() {
                candidates.push(e.path().join("bin/node"));
            }
        }
    }
    candidates.retain(|p| p.exists());
    candidates.sort_by_key(|p| std::cmp::Reverse(node_major(p)));
    if let Some(best) = candidates.into_iter().find(|p| node_major(p) >= 20) {
        return Some(best);
    }
    ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

pub fn spawn(node: &PathBuf) -> std::io::Result<tokio::process::Child> {
    let sandbox = PathBuf::from("/usr/bin/sandbox-exec");
    let mut cmd = if cfg!(target_os = "macos") && sandbox.exists() {
        let mut c = tokio::process::Command::new(sandbox);
        c.arg("-p").arg(SANDBOX_PROFILE).arg(node);
        c
    } else {
        tokio::process::Command::new(node)
    };
    cmd.arg("--permission")
        .arg("--input-type=module")
        .arg("-e")
        .arg(RUNNER)
        .env_clear()
        .env("NODE_NO_WARNINGS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

/// Résultat d'outil MCP simplifié pour le code : `structuredContent` s'il existe, sinon le
/// texte (converti en JSON s'il en est), sinon le contenu brut.
pub fn simplify_result(v: &Value) -> Value {
    if let Some(s) = v.get("structuredContent") {
        return s.clone();
    }
    if let Some(items) = v.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = items
            .iter()
            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect();
        if texts.len() == items.len() && !texts.is_empty() {
            let joined = texts.join("\n");
            return serde_json::from_str(&joined).unwrap_or(Value::String(joined));
        }
        return Value::Array(items.clone());
    }
    v.clone()
}

pub fn tool_definition() -> Value {
    serde_json::json!({
        "name": "mcphub_run",
        "description": "Run JavaScript that chains several MCP tool calls and returns only the final result (saves tokens on multi-step or bulk work). Inside the code: `await mcp.call(server, tool, args)` returns the tool result (structuredContent, or parsed JSON text), `await mcp.search(query, server?)` finds tools, `console.log` is captured, and `return` gives the output. Async top-level code. No filesystem, network or require: only mcp.call/mcp.search. Same permissions as mcphub_call.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Body of an async function, e.g. const p = await mcp.call('supabase','list_projects',{}); return p.length;"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max run time in seconds (5 to 300).",
                    "minimum": 5,
                    "maximum": 300
                }
            },
            "required": ["code"]
        },
        "annotations": { "title": "Run code over MCP tools", "openWorldHint": true }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn simplification_des_resultats() {
        assert_eq!(simplify_result(&json!({"structuredContent": {"a": 1}, "content": []})), json!({"a": 1}));
        assert_eq!(
            simplify_result(&json!({"content": [{"type":"text","text":"[1,2]"}]})),
            json!([1, 2])
        );
        assert_eq!(
            simplify_result(&json!({"content": [{"type":"text","text":"bonjour"}]})),
            json!("bonjour")
        );
    }

    #[test]
    fn reglages() {
        assert!(parse(None).enabled);
        assert!(!parse(Some(&json!({"codeMode": false}))).enabled);
        let c = parse(Some(&json!({"codeMode": {"timeoutSecs": 1000, "maxCalls": 5}})));
        assert_eq!(c.timeout_secs, 300);
        assert_eq!(c.max_calls, 5);
    }

    #[test]
    fn version_node() {
        assert_eq!(node_major(&PathBuf::from("/u/.nvm/versions/node/v25.0.0/bin/node")), 25);
        assert_eq!(node_major(&PathBuf::from("/opt/homebrew/bin/node")), 0);
    }
}
