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

/// Profil du processus de code : ni réseau, ni écriture, ni lecture des secrets, ni programmes
/// d'évasion (en plus de `node --permission`).
fn sandbox_profile() -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let q = |p: &str| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""));
    let mut denied: Vec<String> = crate::sandbox::SECRET_DIRS
        .iter()
        .map(|d| format!("(subpath {})", q(&format!("{}/{}", home, d))))
        .collect();
    denied.extend(
        crate::sandbox::SECRET_FILES
            .iter()
            .map(|f| format!("(literal {})", q(&format!("{}/{}", home, f)))),
    );
    denied.push(format!("(subpath {})", q(&format!("{}/Library/Keychains", home))));
    format!(
        "(version 1)(allow default)(deny network*)(deny file-write*)(allow file-write-data (literal \"/dev/null\"))(deny file-read* {})(deny process-exec (literal \"/usr/bin/security\") (literal \"/bin/launchctl\") (literal \"/usr/bin/osascript\") (literal \"/usr/bin/open\"))",
        denied.join(" ")
    )
}

/// Taille maximale d'une ligne de protocole venant du code (4 Mo).
pub const MAX_LINE: usize = 4 * 1024 * 1024;

/// Lit une ligne sans jamais dépasser `max` octets (protège la mémoire du démon).
pub async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<String>, String> {
    use tokio::io::AsyncBufReadExt;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let buf = r.fill_buf().await.map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return Ok(if out.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&out).to_string())
            });
        }
        if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            if out.len() + pos > max {
                return Err("ligne trop longue".into());
            }
            out.extend_from_slice(&buf[..pos]);
            r.consume(pos + 1);
            return Ok(Some(String::from_utf8_lossy(&out).to_string()));
        }
        let n = buf.len();
        if out.len() + n > max {
            return Err("ligne trop longue".into());
        }
        out.extend_from_slice(buf);
        r.consume(n);
    }
}

/// Version de Node (majeure, mineure) via `node --version`.
pub fn node_version(node: &PathBuf) -> Option<(u32, u32)> {
    let out = std::process::Command::new(node).arg("--version").output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout);
    let mut parts = v.trim().trim_start_matches('v').split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// `--permission` (sans "experimental") existe depuis Node 22.13 / 23.5.
pub fn node_supported(v: (u32, u32)) -> bool {
    v.0 > 23 || (v.0 == 23 && v.1 >= 5) || (v.0 == 22 && v.1 >= 13)
}

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
        return (p.exists() && node_version(&p).map_or(false, node_supported)).then_some(p);
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
    candidates.extend(
        ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"]
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    candidates
        .into_iter()
        .find(|p| node_version(p).map_or(false, node_supported))
}

pub fn spawn(node: &PathBuf) -> std::io::Result<tokio::process::Child> {
    // Sans bac à sable système, pas de code mode (le réseau ne serait bloqué que par Node).
    let sandbox = PathBuf::from("/usr/bin/sandbox-exec");
    if !(cfg!(target_os = "macos") && sandbox.exists()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "sandbox-exec indisponible : code mode refuse",
        ));
    }
    let mut cmd = tokio::process::Command::new(sandbox);
    cmd.arg("-p").arg(sandbox_profile()).arg(node);
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
    fn versions_node_acceptees() {
        assert!(node_supported((25, 0)));
        assert!(node_supported((22, 13)));
        assert!(!node_supported((22, 12)));
        assert!(!node_supported((20, 18)));
        assert!(!node_supported((23, 4)));
    }

    #[tokio::test]
    async fn ligne_bornee() {
        let data = b"court\n".to_vec();
        let mut r = tokio::io::BufReader::new(&data[..]);
        assert_eq!(read_line_limited(&mut r, 10).await.unwrap().as_deref(), Some("court"));
        assert_eq!(read_line_limited(&mut r, 10).await.unwrap(), None);
        let big = vec![b'x'; 100];
        let mut r = tokio::io::BufReader::new(&big[..]);
        assert!(read_line_limited(&mut r, 10).await.is_err());
    }

    #[test]
    fn version_node() {
        assert_eq!(node_major(&PathBuf::from("/u/.nvm/versions/node/v25.0.0/bin/node")), 25);
        assert_eq!(node_major(&PathBuf::from("/opt/homebrew/bin/node")), 0);
    }
}
