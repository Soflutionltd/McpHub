//! Permissions par outil : autoriser, demander confirmation, ou bloquer.
//!
//! Configuration dans `~/.McpHub/config.json` :
//!
//! ```json
//! "settings": {
//!   "permissions": {
//!     "default": "allow",
//!     "autoClassify": true,
//!     "confirmTtlSecs": 300,
//!     "rules": [
//!       { "server": "*", "tool": "*delete*", "action": "confirm" },
//!       { "server": "cloudflare", "tool": "*", "action": "confirm", "reason": "prod" },
//!       { "server": "github", "tool": "delete_repository", "action": "deny" }
//!     ]
//!   }
//! }
//! ```
//!
//! - La première règle qui correspond l'emporte (ordre du fichier).
//! - `*` est un joker ; la comparaison ignore la casse.
//! - `autoClassify` : sans règle, un outil au nom destructeur (delete, remove, drop, purge...)
//!   passe en `confirm` au lieu de la valeur `default`.
//!
//! LIMITE IMPORTANTE : `confirm` renvoie le jeton à l'agent IA, qui doit demander l'accord de
//! l'utilisateur avant de rappeler l'outil. Cela protège contre les erreurs d'un agent honnête,
//! pas contre un agent manipulé (injection de prompt) qui rappellerait tout seul. Pour ce qui
//! est critique (suppression de dépôt, base de production, paiements), utiliser `deny`.
//! Une validation hors bande (tableau de bord, notification macOS) est prévue plus tard.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Confirm,
    Deny,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "autoriser" => Some(Action::Allow),
            "confirm" | "ask" | "confirmer" => Some(Action::Confirm),
            "deny" | "block" | "bloquer" | "refuser" => Some(Action::Deny),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Confirm => "confirm",
            Action::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub server: String,
    pub tool: String,
    pub action: Action,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionsConfig {
    pub default: Action,
    pub auto_classify: bool,
    pub confirm_ttl_secs: u64,
    pub rules: Vec<Rule>,
    /// `confirm` = fenêtre macOS (seul un humain peut cliquer) ; repli sur le jeton
    /// si aucune interface graphique n'est disponible. `confirmVia: "token"` pour forcer le jeton.
    pub confirm_via_dialog: bool,
    /// `confirmVia: "auto"` : fenêtre, et jeton si aucune interface n'est disponible.
    /// Par défaut (`"dialog"`), pas de fenêtre possible = refus (fermé par défaut).
    pub dialog_fallback_token: bool,
    pub dialog_timeout_secs: u64,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            default: Action::Allow,
            auto_classify: false,
            confirm_ttl_secs: 300,
            rules: Vec::new(),
            confirm_via_dialog: true,
            dialog_fallback_token: false,
            dialog_timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// D'où vient la décision : "rule#N", "auto:destructive" ou "default".
    pub source: String,
    pub reason: Option<String>,
}

impl PermissionsConfig {
    pub fn from_json(value: &Value) -> PermissionsConfig {
        let mut cfg = PermissionsConfig::default();
        if let Some(d) = value
            .get("default")
            .and_then(|v| v.as_str())
            .and_then(Action::parse)
        {
            cfg.default = d;
        }
        if let Some(a) = value.get("autoClassify").and_then(|v| v.as_bool()) {
            cfg.auto_classify = a;
        }
        if let Some(t) = value.get("confirmTtlSecs").and_then(|v| v.as_u64()) {
            cfg.confirm_ttl_secs = t.clamp(30, 3600);
        }
        if let Some(v) = value.get("confirmVia").and_then(|v| v.as_str()) {
            cfg.confirm_via_dialog = !v.eq_ignore_ascii_case("token");
            cfg.dialog_fallback_token = v.eq_ignore_ascii_case("auto");
        }
        if let Some(t) = value.get("dialogTimeoutSecs").and_then(|v| v.as_u64()) {
            cfg.dialog_timeout_secs = t.clamp(10, 600);
        }
        if let Some(rules) = value.get("rules").and_then(|v| v.as_array()) {
            for (i, r) in rules.iter().enumerate() {
                let action = match r
                    .get("action")
                    .and_then(|v| v.as_str())
                    .and_then(Action::parse)
                {
                    Some(a) => a,
                    None => {
                        eprintln!(
                            "[McpHub][WARN] permissions: regle #{} ignoree (action invalide)",
                            i + 1
                        );
                        continue;
                    }
                };
                cfg.rules.push(Rule {
                    server: r
                        .get("server")
                        .and_then(|v| v.as_str())
                        .unwrap_or("*")
                        .to_string(),
                    tool: r
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("*")
                        .to_string(),
                    action,
                    reason: r.get("reason").and_then(|v| v.as_str()).map(String::from),
                });
            }
        }
        cfg
    }

    pub fn decide(&self, server: &str, tool: &str) -> Decision {
        self.decide_with(server, tool, false)
    }

    /// Comme `decide`, en tenant compte de l'annotation MCP `destructiveHint` déclarée par
    /// le serveur. Une annotation ne peut que durcir la décision (jamais l'assouplir) :
    /// `readOnlyHint` est déclaratif et ne doit pas permettre de contourner une règle.
    pub fn decide_with(&self, server: &str, tool: &str, declared_destructive: bool) -> Decision {
        for (i, rule) in self.rules.iter().enumerate() {
            if glob_match(&rule.server, server) && glob_match(&rule.tool, tool) {
                return Decision {
                    action: rule.action,
                    source: format!("rule#{}", i + 1),
                    reason: rule.reason.clone(),
                };
            }
        }
        if self.auto_classify && is_destructive(tool) && self.default != Action::Deny {
            return Decision {
                action: Action::Confirm,
                source: "auto:destructive".into(),
                reason: Some("nom d'outil destructeur".into()),
            };
        }
        if self.auto_classify && declared_destructive && self.default != Action::Deny {
            return Decision {
                action: Action::Confirm,
                source: "auto:annotation".into(),
                reason: Some("le serveur declare cet outil destructeur (destructiveHint)".into()),
            };
        }
        Decision {
            action: self.default,
            source: "default".into(),
            reason: None,
        }
    }
}

/// Joker `*` (n'importe quelle suite de caractères), insensible à la casse.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

const DESTRUCTIVE_WORDS: &[&str] = &[
    "delete",
    "remove",
    "rm",
    "destroy",
    "drop",
    "purge",
    "wipe",
    "erase",
    "truncate",
    "revoke",
    "terminate",
    "kill",
    "uninstall",
    "reset",
    "unlink",
    "overwrite",
    "deprovision",
    "suspend",
];

/// Découpe le nom (snake_case, kebab-case, camelCase, points) et cherche un verbe destructeur.
pub fn is_destructive(tool: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for c in tool.chars() {
        if c == '_' || c == '-' || c == '.' || c == '/' || c == ' ' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
        .iter()
        .any(|w| DESTRUCTIVE_WORDS.contains(&w.as_str()))
}

// ---------------------------------------------------------------------------
// Jetons de confirmation : un jeton = un appel précis (serveur, outil, arguments),
// usage unique, durée limitée.
// ---------------------------------------------------------------------------

pub struct ConfirmStore {
    pending: Mutex<HashMap<String, (String, Instant)>>,
}

impl ConfirmStore {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self, fingerprint: &str, ttl_secs: u64) -> String {
        let token = random_token();
        let expires = Instant::now() + Duration::from_secs(ttl_secs);
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        map.retain(|_, (_, exp)| *exp > now);
        map.insert(token.clone(), (fingerprint.to_string(), expires));
        token
    }

    /// Consomme le jeton s'il est valide pour exactement cet appel.
    pub fn consume(&self, token: &str, fingerprint: &str) -> bool {
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(token) {
            Some((fp, exp)) if fp == fingerprint && *exp > Instant::now() => {
                map.remove(token);
                true
            }
            Some((_, exp)) if *exp <= Instant::now() => {
                map.remove(token);
                false
            }
            _ => false,
        }
    }
}

/// Empreinte stable d'un appel. serde_json trie les clés des objets (pas de preserve_order),
/// donc deux arguments identiques donnent la même chaîne.
pub fn fingerprint(server: &str, tool: &str, arguments: &Value) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        server.to_lowercase(),
        tool,
        serde_json::to_string(arguments).unwrap_or_default()
    )
}

fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let addr = &buf as *const _ as usize as u128;
        let mixed = nanos ^ (addr << 17) ^ (std::process::id() as u128) << 64;
        buf.copy_from_slice(&mixed.to_le_bytes());
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `McpHub policy test SERVEUR OUTIL` : affiche la décision sans rien appeler.
pub fn run_cli(args: &[String], cfg: &PermissionsConfig) {
    match (args.first().map(|s| s.as_str()), args.get(1), args.get(2)) {
        (Some("test"), Some(server), Some(tool)) => {
            let d = cfg.decide(server, tool);
            println!(
                "{} / {} -> {} ({}{})",
                server,
                tool,
                d.action.as_str(),
                d.source,
                d.reason.map(|r| format!(", {}", r)).unwrap_or_default()
            );
        }
        (Some("show"), _, _) | (None, _, _) => {
            println!(
                "default={} autoClassify={} confirmTtlSecs={}",
                cfg.default.as_str(),
                cfg.auto_classify,
                cfg.confirm_ttl_secs
            );
            if cfg.rules.is_empty() {
                println!("Aucune regle (settings.permissions.rules).");
            }
            for (i, r) in cfg.rules.iter().enumerate() {
                println!(
                    "  #{} server={} tool={} -> {}{}",
                    i + 1,
                    r.server,
                    r.tool,
                    r.action.as_str(),
                    r.reason
                        .as_ref()
                        .map(|x| format!(" ({})", x))
                        .unwrap_or_default()
                );
            }
        }
        _ => eprintln!("Usage: McpHub policy [show] | McpHub policy test SERVEUR OUTIL"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn glob() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*delete*", "github_delete_repo"));
        assert!(glob_match("GitHub", "github"));
        assert!(glob_match("list_*", "list_issues"));
        assert!(!glob_match("list_*", "get_list"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn destructive_detection() {
        assert!(is_destructive("delete_repository"));
        assert!(is_destructive("deleteFile"));
        assert!(is_destructive("dns-records-remove"));
        assert!(is_destructive("purge_cache"));
        assert!(!is_destructive("list_deleted_items"));
        assert!(!is_destructive("get_issue"));
        assert!(!is_destructive("format_document"));
    }

    #[test]
    fn annotation_durcit_mais_n_assouplit_pas() {
        let mut cfg = PermissionsConfig::default();
        cfg.auto_classify = true;
        let d = cfg.decide_with("srv", "apply_changes", true);
        assert_eq!(d.action, Action::Confirm);
        assert_eq!(d.source, "auto:annotation");
        assert_eq!(cfg.decide_with("srv", "apply_changes", false).action, Action::Allow);
        // Une regle explicite reste prioritaire.
        cfg.rules.push(Rule { server: "srv".into(), tool: "apply_*".into(), action: Action::Allow, reason: None });
        assert_eq!(cfg.decide_with("srv", "apply_changes", true).action, Action::Allow);
        // Sans auto_classify, pas d'effet.
        let cfg2 = PermissionsConfig::default();
        assert_eq!(cfg2.decide_with("srv", "x", true).action, Action::Allow);
    }

    #[test]
    fn first_rule_wins_then_auto_then_default() {
        let cfg = PermissionsConfig::from_json(&json!({
            "default": "allow",
            "autoClassify": true,
            "rules": [
                { "server": "github", "tool": "delete_repository", "action": "deny" },
                { "server": "cloudflare", "tool": "*", "action": "confirm", "reason": "prod" },
                { "server": "*", "tool": "list_*", "action": "allow" }
            ]
        }));
        assert_eq!(
            cfg.decide("github", "delete_repository").action,
            Action::Deny
        );
        let d = cfg.decide("Cloudflare", "list_zones");
        assert_eq!(d.action, Action::Confirm);
        assert_eq!(d.source, "rule#2");
        assert_eq!(cfg.decide("supabase", "drop_table").action, Action::Confirm);
        assert_eq!(
            cfg.decide("supabase", "drop_table").source,
            "auto:destructive"
        );
        assert_eq!(cfg.decide("supabase", "list_tables").action, Action::Allow);
        assert_eq!(cfg.decide("supabase", "execute_sql").action, Action::Allow);
    }

    #[test]
    fn invalid_rules_are_skipped_and_defaults_kept() {
        let cfg = PermissionsConfig::from_json(&json!({
            "rules": [{ "server": "x", "action": "maybe" }]
        }));
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.default, Action::Allow);
        assert!(!cfg.auto_classify);
        assert_eq!(cfg.decide("x", "delete_all").action, Action::Allow);
    }

    #[test]
    fn tokens_are_single_use_and_bound_to_the_call() {
        let store = ConfirmStore::new();
        let fp = fingerprint("github", "delete_repo", &json!({"name": "x", "a": 1}));
        let same = fingerprint("GitHub", "delete_repo", &json!({"a": 1, "name": "x"}));
        assert_eq!(fp, same);
        let other = fingerprint("github", "delete_repo", &json!({"name": "y", "a": 1}));
        let token = store.issue(&fp, 300);
        assert_eq!(token.len(), 32);
        assert!(!store.consume(&token, &other));
        assert!(store.consume(&token, &fp));
        assert!(!store.consume(&token, &fp));
    }
}
