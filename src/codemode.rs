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

use std::collections::{HashMap, HashSet};
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
    denied.extend(crate::instance::sandbox_hub_rules(&home));
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

/// Règles de projet appliquées à une exécution de `mcphub_run` (argument `policy`, ajouté par
/// le client McpHub, pas par le modèle). Les noms de serveur sont comparés après la même
/// normalisation que la résolution des serveurs : ni la casse, ni les tirets, ni les
/// underscores ne permettent de contourner une règle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunPolicy {
    /// Présent : seuls ces serveurs sont joignables.
    allowed: Option<HashSet<String>>,
    /// Toujours refusés, même s'ils figurent dans `allowed`.
    denied: HashSet<String>,
    /// Par serveur : arguments dont la valeur, si elle est fournie, doit être celle-ci.
    pinned: HashMap<String, serde_json::Map<String, Value>>,
}

fn policy_server_list(policy: &serde_json::Map<String, Value>, key: &str) -> Result<Option<HashSet<String>>, String> {
    let Some(value) = policy.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(format!("{} must be an array of server names", key));
    };
    let mut names = HashSet::new();
    for item in items {
        let name = item
            .as_str()
            .map(crate::proxy::normalize_server_key)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("{} must be an array of server names", key))?;
        names.insert(name);
    }
    Ok(Some(names))
}

impl RunPolicy {
    /// Tout écart de forme est une erreur (clé inconnue comprise) : une règle mal écrite ne
    /// doit jamais être ignorée en silence.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let Some(object) = value.as_object() else {
            return Err("policy must be an object".into());
        };
        if let Some(unknown) = object
            .keys()
            .find(|k| !["allowed_servers", "denied_servers", "pinned_args"].contains(&k.as_str()))
        {
            return Err(format!("unknown policy key \"{}\"", unknown));
        }
        let mut policy = Self {
            allowed: policy_server_list(object, "allowed_servers")?,
            denied: policy_server_list(object, "denied_servers")?.unwrap_or_default(),
            pinned: HashMap::new(),
        };
        if let Some(pinned) = object.get("pinned_args") {
            let Some(servers) = pinned.as_object() else {
                return Err("pinned_args must be an object of { server: { key: value } }".into());
            };
            for (server, args) in servers {
                let key = crate::proxy::normalize_server_key(server);
                let Some(args) = args.as_object().filter(|_| !key.is_empty()) else {
                    return Err(format!("pinned_args for server \"{}\" must be an object", server));
                };
                if policy.pinned.insert(key, args.clone()).is_some() {
                    return Err(format!("pinned_args names server \"{}\" twice", server));
                }
            }
        }
        Ok(policy)
    }

    /// Le serveur est-il joignable sous cette règle ?
    pub fn check_server(&self, server: &str) -> Result<(), String> {
        let key = crate::proxy::normalize_server_key(server);
        let allowed = self.allowed.as_ref().is_none_or(|list| list.contains(&key));
        if self.denied.contains(&key) || !allowed {
            return Err(format!("Server \"{}\" is not allowed by the project policy.", server));
        }
        Ok(())
    }

    /// Contrôle d'un `mcp.call` : serveur joignable, et aucun argument épinglé contredit.
    /// Un argument épinglé absent de l'appel n'est pas ajouté : l'appel part tel quel.
    pub fn check_call(&self, server: &str, args: &Value) -> Result<(), String> {
        self.check_server(server)?;
        let Some(pinned) = self.pinned.get(&crate::proxy::normalize_server_key(server)) else {
            return Ok(());
        };
        for (key, expected) in pinned {
            if args.get(key).is_some_and(|given| given != expected) {
                return Err(format!("{} mismatch for server \"{}\"", key, server));
            }
        }
        Ok(())
    }

    /// Résultats de `mcp.search` : seuls les outils de serveurs joignables restent. Un
    /// résultat sans serveur lisible est retiré.
    pub fn filter_search_results(&self, results: &Value, limit: usize) -> Value {
        let kept = results
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("server")
                            .and_then(|s| s.as_str())
                            .is_some_and(|server| self.check_server(server).is_ok())
                    })
                    .take(limit)
                    .cloned()
                    .collect::<Vec<Value>>()
            })
            .unwrap_or_default();
        Value::Array(kept)
    }
}

/// Résultat de `mcphub_run` renvoyé à l'appelant. `"policy": "enforced"` n'apparaît que si une
/// règle de projet a été fournie et appliquée à cette exécution.
pub fn run_payload(calls: usize, duration_ms: u64, logs: Value, policy_enforced: bool) -> Value {
    let mut payload = serde_json::json!({ "calls": calls, "duration_ms": duration_ms, "logs": logs });
    if policy_enforced {
        payload["policy"] = serde_json::json!("enforced");
    }
    payload
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
                },
                "policy": {
                    "type": "object",
                    "description": "Set by the McpHub client, not by the model."
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

    fn policy(value: Value) -> RunPolicy {
        RunPolicy::from_json(&value).expect("policy valide")
    }

    #[test]
    fn policy_allowed_servers() {
        let p = policy(json!({"allowed_servers": ["supabase", "Cloudflare-Docs"]}));
        assert!(p.check_call("supabase", &json!({})).is_ok());
        assert_eq!(
            p.check_call("github", &json!({})).unwrap_err(),
            "Server \"github\" is not allowed by the project policy."
        );
        // Meme normalisation que la resolution des serveurs : casse, tirets, underscores.
        assert!(p.check_call("SUPABASE", &json!({})).is_ok());
        assert!(p.check_call("cloudflare_docs", &json!({})).is_ok());
        assert!(p.check_call("mcphub", &json!({})).is_err(), "les outils internes aussi");
        assert!(p.check_call("", &json!({})).is_err());
        // Liste vide : rien n'est joignable.
        assert!(policy(json!({"allowed_servers": []})).check_server("supabase").is_err());
        // Sans policy de serveurs : tout passe.
        assert!(policy(json!({})).check_call("github", &json!({"a": 1})).is_ok());
    }

    #[test]
    fn policy_denied_servers() {
        let p = policy(json!({"denied_servers": ["stripe"]}));
        assert!(p.check_call("supabase", &json!({})).is_ok());
        for alias in ["stripe", "Stripe", "STRIPE", "s-tripe", "stri_pe"] {
            assert_eq!(
                p.check_call(alias, &json!({})).unwrap_err(),
                format!("Server \"{}\" is not allowed by the project policy.", alias)
            );
        }
        // Refuse meme s'il figure aussi dans allowed_servers.
        let p = policy(json!({"allowed_servers": ["stripe", "supabase"], "denied_servers": ["stripe"]}));
        assert!(p.check_server("stripe").is_err());
        assert!(p.check_server("supabase").is_ok());
    }

    #[test]
    fn policy_pinned_args() {
        let p = policy(json!({"pinned_args": {"supabase": {"project_id": "xyz"}}}));
        assert!(p.check_call("supabase", &json!({"project_id": "xyz", "query": "select 1"})).is_ok());
        assert_eq!(
            p.check_call("supabase", &json!({"project_id": "autre"})).unwrap_err(),
            "project_id mismatch for server \"supabase\""
        );
        assert!(p.check_call("Supa-Base", &json!({"project_id": "autre"})).is_err());
        assert!(p.check_call("supabase", &json!({"project_id": null})).is_err());
        // Cle absente : l'appel passe tel quel.
        assert!(p.check_call("supabase", &json!({"query": "select 1"})).is_ok());
        assert!(p.check_call("supabase", &json!(null)).is_ok());
        // Les autres serveurs ne sont pas concernes.
        assert!(p.check_call("github", &json!({"project_id": "autre"})).is_ok());
    }

    #[test]
    fn policy_filtre_les_resultats_de_recherche() {
        let results = json!([
            {"server": "supabase", "tool": "list_tables"},
            {"server": "stripe", "tool": "list_customers"},
            {"server": "Supabase", "tool": "execute_sql"},
            {"server": "github", "tool": "list_issues"},
            {"tool": "sans_serveur"},
            "texte"
        ]);
        let allowed = policy(json!({"allowed_servers": ["supabase", "stripe"], "denied_servers": ["stripe"]}));
        assert_eq!(
            allowed.filter_search_results(&results, 10),
            json!([
                {"server": "supabase", "tool": "list_tables"},
                {"server": "Supabase", "tool": "execute_sql"}
            ])
        );
        assert_eq!(allowed.filter_search_results(&results, 1).as_array().unwrap().len(), 1);
        let denied = policy(json!({"denied_servers": ["stripe"]}));
        let kept = denied.filter_search_results(&results, 10);
        assert_eq!(kept.as_array().unwrap().len(), 3);
        assert!(kept.as_array().unwrap().iter().all(|r| r["server"] != "stripe"));
        // Une reponse qui n'est pas une liste ne laisse rien passer.
        assert_eq!(allowed.filter_search_results(&json!({"available_servers": ["github"]}), 10), json!([]));
        // Filtre `server` de mcp.search hors liste : erreur.
        assert!(allowed.check_server("github").is_err());
        assert!(allowed.check_server("stripe").is_err());
    }

    #[test]
    fn policy_mal_formee_refusee() {
        for invalid in [
            json!(null),
            json!("supabase"),
            json!(["supabase"]),
            json!({"allowed_servers": "supabase"}),
            json!({"allowed_servers": [1]}),
            json!({"allowed_servers": [""]}),
            json!({"denied_servers": {"stripe": true}}),
            json!({"denied_servers": [null]}),
            json!({"pinned_args": ["supabase"]}),
            json!({"pinned_args": {"supabase": "xyz"}}),
            json!({"pinned_args": {"supabase": {"a": 1}, "Supa-base": {"a": 2}}}),
            json!({"allowed_server": ["supabase"]}),
        ] {
            assert!(RunPolicy::from_json(&invalid).is_err(), "{} doit etre refusee", invalid);
        }
    }

    #[test]
    fn policy_declaree_dans_le_schema_de_mcphub_run() {
        let def = tool_definition();
        let policy = &def["inputSchema"]["properties"]["policy"];
        assert_eq!(policy["type"], "object");
        assert_eq!(policy["description"], "Set by the McpHub client, not by the model.");
        assert_eq!(def["inputSchema"]["required"], json!(["code"]), "policy reste optionnelle");
    }

    #[test]
    fn policy_enforced_seulement_si_appliquee() {
        let with = run_payload(2, 15, json!(["a"]), true);
        assert_eq!(with, json!({"calls": 2, "duration_ms": 15, "logs": ["a"], "policy": "enforced"}));
        let without = run_payload(2, 15, json!([]), false);
        assert!(without.get("policy").is_none());
        assert_eq!(without, json!({"calls": 2, "duration_ms": 15, "logs": []}));
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
