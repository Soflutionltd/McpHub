//! Isolation des serveurs MCP enfants avec le bac à sable natif de macOS (`sandbox-exec`).
//!
//! Pourquoi : un serveur MCP est un programme tiers (souvent un paquet npm récupéré à la volée)
//! qui tourne avec tous les droits de l'utilisateur. Un paquet compromis peut lire les clés SSH,
//! les jetons cloud, les cookies du navigateur ou la config McpHub. Docker isolerait mieux mais
//! casserait la plupart des serveurs (Xcode, simulateurs, navigateur, fichiers locaux) ; le bac
//! à sable macOS s'applique au processus et à tous ses descendants, sans conteneur.
//!
//! Deux niveaux :
//! - `shield` (bouclier) : tout reste permis SAUF lire ou écrire les secrets (SSH, GPG, clouds,
//!   jetons npm/GitHub, profils et cookies des navigateurs, configs Claude/Cursor, McpHub).
//!   Compatible avec presque tous les serveurs.
//! - `strict` : bouclier + aucune écriture hors des dossiers temporaires et caches (+ `allowWrite`),
//!   et réseau coupé si `network: false`.
//!
//! Les chemins que le serveur référence lui-même (variables d'environnement, arguments) restent
//! lisibles : une clé .p8 passée en variable continue de fonctionner.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Shield,
    Strict,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxSpec {
    pub mode: SandboxMode,
    pub network: bool,
    pub allow_read: Vec<String>,
    pub allow_write: Vec<String>,
    /// Programmes sensibles autorisés malgré tout (ex. "osascript", "open" pour un serveur
    /// qui pilote le simulateur iOS).
    pub allow_exec: Vec<String>,
}

/// Secrets jamais lisibles par un serveur isolé (relatifs au dossier personnel).
pub const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".config/op",
    ".kube",
    ".docker",
    ".McpHub",
    ".mcp-auth",
    ".xurl",
    ".private_keys",
    ".cursor",
    ".claude",
    ".codeium",
    ".password-store",
    ".supabase",
    ".config/supabase",
    ".config/stripe",
    ".config/github-copilot",
    ".wrangler",
    ".terraform.d",
    ".codex",
    ".gemini",
    ".cargo/credentials.toml",
    "Library/LaunchAgents",
    "Library/Group Containers/2BUA8C4S2C.com.1password",
    "Library/Application Support/Code/User",
    "Library/Application Support/Zed",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
    "Library/Application Support/Claude",
    "Library/Application Support/Cursor",
    "Library/Application Support/1Password",
    "Library/Cookies",
    "Library/Safari",
    "Library/Messages",
    "Library/Mail",
];

pub const SECRET_FILES: &[&str] = &[
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".claude.json",
    ".zsh_history",
    ".bash_history",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".vault-token",
];

/// Lisibles (git, flutter... en ont besoin) mais jamais modifiables : un alias ou un hook git
/// ajouté par un serveur s'exécuterait plus tard hors du bac à sable.
const WRITE_PROTECTED: &[&str] = &[".gitconfig", ".config/git", ".gitignore_global"];

/// Programmes qui permettent de sortir du bac à sable ou de lire le Trousseau :
/// interdits aux serveurs isolés (sauf `allowExec`).
const BLOCKED_EXEC: &[(&str, &str)] = &[
    ("security", "/usr/bin/security"),
    ("launchctl", "/bin/launchctl"),
    ("osascript", "/usr/bin/osascript"),
    ("open", "/usr/bin/open"),
    ("automator", "/usr/bin/automator"),
    ("shortcuts", "/usr/bin/shortcuts"),
];

/// Serveurs conçus pour tout faire sur la machine (shell, fichiers) : pas de bac à sable par
/// défaut, sauf réglage explicite sur le serveur.
const UNSANDBOXED_BY_DEFAULT: &[&str] = &["desktop-commander"];

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn mode_of(s: &str) -> Option<SandboxMode> {
    match s.to_ascii_lowercase().as_str() {
        "shield" | "bouclier" | "on" | "true" => Some(SandboxMode::Shield),
        "strict" => Some(SandboxMode::Strict),
        _ => None,
    }
}

/// Réglage effectif d'un serveur : `mcpServers.X.sandbox` (false / true / "shield" / "strict" /
/// objet), sinon `settings.sandbox` global, sinon pas d'isolation.
pub fn resolve(server_name: &str, server: &Value, global: Option<&Value>, command: &str, args: &[String]) -> Option<SandboxSpec> {
    let local = server.get("sandbox");
    let explicit = local.is_some();
    let spec_value = match local {
        Some(Value::Bool(false)) => return None,
        Some(v) => v.clone(),
        None => match global {
            Some(Value::Bool(false)) | None => return None,
            Some(g) => g.clone(),
        },
    };
    if !explicit {
        let haystack = format!("{} {} {}", server_name, command, args.join(" ")).to_lowercase();
        if UNSANDBOXED_BY_DEFAULT.iter().any(|n| haystack.contains(n)) {
            return None;
        }
    }
    let mut spec = SandboxSpec {
        mode: SandboxMode::Shield,
        network: true,
        allow_read: Vec::new(),
        allow_write: Vec::new(),
        allow_exec: Vec::new(),
    };
    match &spec_value {
        Value::Bool(true) => {}
        Value::String(s) => spec.mode = mode_of(s)?,
        Value::Object(o) => {
            if let Some(m) = o.get("mode").and_then(|v| v.as_str()) {
                spec.mode = mode_of(m)?;
            }
            if let Some(n) = o.get("network").and_then(|v| v.as_bool()) {
                spec.network = n;
            }
            spec.allow_read = str_list(o.get("allowRead"));
            spec.allow_write = str_list(o.get("allowWrite"));
            spec.allow_exec = str_list(o.get("allowExec"));
        }
        _ => return None,
    }
    // Besoins connus des enveloppes courantes.
    let cmd_base = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    if args.iter().any(|a| a.contains("mcp-remote")) {
        spec.allow_write.push("~/.mcp-auth".into());
    }
    if cmd_base == "xurl" || args.iter().any(|a| a.contains("@xdevplatform/xurl")) {
        spec.allow_write.push("~/.xurl".into());
    }
    Some(spec)
}

fn expand(p: &str, home: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{}", home, rest)
    } else if p == "~" {
        home.to_string()
    } else {
        p.to_string()
    }
}

/// Vrai si `p` contient un secret (ex. ~ ou /Users) : ne doit jamais être rouvert en bloc.
fn is_ancestor_of_secret(p: &str, home: &str) -> bool {
    let p = p.trim_end_matches('/');
    SECRET_DIRS
        .iter()
        .chain(SECRET_FILES.iter())
        .map(|d| format!("{}/{}", home, d))
        .any(|d| d == p || d.starts_with(&format!("{}/", p)) || p.is_empty())
}

fn q(p: &str) -> String {
    format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Chemin `p` strictement à l'intérieur d'un élément secret (ex. une clé .p8 dans
/// ~/.private_keys). Jamais un dossier parent d'un secret (~, /Users...) ni le secret lui-même :
/// sinon une variable `WORKSPACE=~` rouvrirait tout le bouclier.
fn strictly_inside_secret(p: &str, home: &str) -> bool {
    let p = p.trim_end_matches('/');
    if p.contains("/../") || p.ends_with("/..") {
        return false;
    }
    SECRET_DIRS
        .iter()
        .map(|d| format!("{}/{}", home, d))
        .any(|d| p.len() > d.len() + 1 && p.starts_with(&d) && p.as_bytes()[d.len()] == b'/')
}

/// Chemins secrets que le serveur référence lui-même (variables d'environnement, arguments)
/// et qui restent donc lisibles, en lecture seule et un par un.
pub fn referenced_paths(env: &HashMap<String, String>, args: &[String], home: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: &str| {
        for part in v.split(':') {
            let p = expand(part.trim(), home);
            if p.starts_with('/') && strictly_inside_secret(&p, home) && !out.contains(&p) {
                out.push(p);
            }
        }
    };
    for (k, v) in env {
        if k != "PATH" {
            push(v);
        }
    }
    for a in args {
        let a = a.split_once('=').map(|(_, v)| v).unwrap_or(a);
        push(a);
    }
    out
}

/// Profil SBPL. Dans ce langage, la dernière règle qui correspond l'emporte : on interdit
/// d'abord, puis on rouvre ce que le serveur référence ou ce qui est autorisé explicitement.
pub fn profile(spec: &SandboxSpec, home: &str, referenced: &[String]) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    let mut denied: Vec<String> = SECRET_DIRS
        .iter()
        .map(|d| format!("(subpath {})", q(&format!("{}/{}", home, d))))
        .collect();
    denied.extend(
        SECRET_FILES
            .iter()
            .map(|f| format!("(literal {})", q(&format!("{}/{}", home, f)))),
    );
    p.push_str(&format!("(deny file-read* file-write* {})\n", denied.join(" ")));
    let protected: Vec<String> = WRITE_PROTECTED
        .iter()
        .map(|f| format!("(subpath {})", q(&format!("{}/{}", home, f))))
        .collect();
    p.push_str(&format!("(deny file-write* {})\n", protected.join(" ")));

    if spec.mode == SandboxMode::Strict {
        p.push_str("(deny file-write*)\n");
        let mut writable: Vec<String> = vec![
            "(subpath \"/private/tmp\")".into(),
            "(subpath \"/private/var/folders\")".into(),
            "(subpath \"/dev\")".into(),
        ];
        for d in [".npm", ".cache", "Library/Caches", ".bun", ".pnpm-store", "Library/pnpm"] {
            writable.push(format!("(subpath {})", q(&format!("{}/{}", home, d))));
        }
        p.push_str(&format!("(allow file-write* {})\n", writable.join(" ")));
    }
    // Sorties du bac à sable : lecture du Trousseau, lancement de tâches, pilotage d'apps.
    let blocked: Vec<String> = BLOCKED_EXEC
        .iter()
        .filter(|(name, _)| !spec.allow_exec.iter().any(|a| a == name))
        .map(|(_, path)| format!("(literal {})", q(path)))
        .collect();
    if !blocked.is_empty() {
        p.push_str(&format!("(deny process-exec {})\n", blocked.join(" ")));
    }
    if !spec.network {
        p.push_str("(deny network*)\n");
    }
    // Exceptions en lecture : uniquement des chemins précis à l'intérieur d'un secret.
    let reads: Vec<String> = referenced
        .iter()
        .cloned()
        .chain(spec.allow_read.iter().map(|r| expand(r, home)))
        .filter(|r| r.starts_with('/') && strictly_inside_secret(r, home))
        .map(|r| format!("(subpath {})", q(&r)))
        .collect();
    if !reads.is_empty() {
        p.push_str(&format!("(allow file-read* {})\n", reads.join(" ")));
    }
    let writes: Vec<String> = spec
        .allow_write
        .iter()
        .map(|w| expand(w, home))
        .filter(|w| {
            w.starts_with('/')
                && (!is_ancestor_of_secret(w, home)
                    || SECRET_DIRS.iter().any(|d| *w == format!("{}/{}", home, d)))
        })
        .map(|w| format!("(subpath {})", q(&w)))
        .collect();
    if !writes.is_empty() {
        p.push_str(&format!("(allow file-read* file-write* {})\n", writes.join(" ")));
    }
    p
}

/// Commande enveloppée : `/usr/bin/sandbox-exec -p PROFIL commande args...`.
/// Hors macOS (ou sans sandbox-exec), la commande est lancée telle quelle avec un avertissement.
pub fn wrap(
    server: &str,
    spec: Option<&SandboxSpec>,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> (String, Vec<String>) {
    let Some(spec) = spec else {
        return (command.to_string(), args.to_vec());
    };
    let exe = "/usr/bin/sandbox-exec";
    if !cfg!(target_os = "macos") || !Path::new(exe).exists() {
        eprintln!("[McpHub][SANDBOX] {} : sandbox-exec indisponible, serveur lance sans isolation", server);
        return (command.to_string(), args.to_vec());
    }
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let referenced = referenced_paths(env, args, &home);
    let mut new_args = vec!["-p".to_string(), profile(spec, &home, &referenced), command.to_string()];
    new_args.extend(args.iter().cloned());
    (exe.to_string(), new_args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolution_des_reglages() {
        let g = json!("shield");
        assert!(resolve("supabase", &json!({}), Some(&g), "npx", &[]).is_some());
        assert!(resolve("supabase", &json!({"sandbox": false}), Some(&g), "npx", &[]).is_none());
        assert!(resolve("supabase", &json!({}), None, "npx", &[]).is_none());
        assert!(resolve("dc", &json!({}), Some(&g), "npx", &["@wonderwhy-er/desktop-commander".into()]).is_none());
        assert!(resolve("dc", &json!({"sandbox": "shield"}), Some(&g), "npx", &["desktop-commander".into()]).is_some());
        let s = resolve("x", &json!({"sandbox": {"mode": "strict", "network": false, "allowWrite": ["~/data"]}}), None, "xurl", &[]).unwrap();
        assert_eq!(s.mode, SandboxMode::Strict);
        assert!(!s.network);
        assert!(s.allow_write.contains(&"~/data".to_string()));
        assert!(s.allow_write.contains(&"~/.xurl".to_string()));
        let r = resolve("cf", &json!({}), Some(&g), "npx", &["-y".into(), "mcp-remote@0.15.1".into()]).unwrap();
        assert!(r.allow_write.contains(&"~/.mcp-auth".to_string()));
    }

    #[test]
    fn profil_bouclier_et_exceptions() {
        let spec = SandboxSpec { mode: SandboxMode::Shield, network: true, allow_read: vec![], allow_write: vec![], allow_exec: vec![] };
        let env = HashMap::from([("KEY".to_string(), "/Users/a/.private_keys/k.p8".to_string())]);
        let refs = referenced_paths(&env, &["--file=/Users/a/doc.txt".into()], "/Users/a");
        assert!(refs.contains(&"/Users/a/.private_keys/k.p8".to_string()));
        assert!(!refs.contains(&"/Users/a/doc.txt".to_string()), "pas besoin, deja lisible");
        // Un dossier parent d'un secret ne rouvre jamais le bouclier.
        for evil in ["~", "/Users/a", "/Users", "/", "~/.config", "/Users/a/.ssh", "/Users/a/.ssh/../.ssh/id_rsa"] {
            let env = HashMap::from([("W".to_string(), evil.to_string())]);
            assert!(referenced_paths(&env, &[], "/Users/a").is_empty(), "{}", evil);
        }
        let p = profile(&spec, "/Users/a", &refs);
        assert!(p.contains("(deny file-read* file-write* (subpath \"/Users/a/.ssh\")"));
        assert!(p.contains("(deny process-exec (literal \"/usr/bin/security\")"));
        let deny_at = p.find("(deny file-read*").unwrap();
        let allow_at = p.find("(allow file-read* (subpath \"/Users/a/.private_keys/k.p8\")").unwrap();
        assert!(allow_at > deny_at, "l'exception doit suivre l'interdiction");
        assert!(!p.contains("(deny file-write*)\n"));
        assert!(!p.contains("network-outbound"));
    }

    #[test]
    fn profil_strict() {
        let spec = SandboxSpec { mode: SandboxMode::Strict, network: false, allow_read: vec![], allow_write: vec!["~/out".into(), "~".into()], allow_exec: vec!["open".into()] };
        let p = profile(&spec, "/Users/a", &[]);
        assert!(p.contains("(deny file-write*)\n"));
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("(subpath \"/Users/a\"))"), "~ ne doit pas etre rouvert");
        assert!(!p.contains("/usr/bin/open"));
        assert!(p.contains("(allow file-read* file-write* (subpath \"/Users/a/out\"))"));
    }

    #[test]
    fn guillemets_echappes() {
        assert_eq!(q("/a \"b\""), "\"/a \\\"b\\\"\"");
    }
}
