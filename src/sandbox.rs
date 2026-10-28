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
const UNSANDBOXED_BY_DEFAULT: &[&str] = &["desktop-commander", "kilnshell"];

fn str_list(server_name: &str, key: &str, v: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    v.as_array()
        .and_then(|a| a.iter().map(|s| s.as_str().map(String::from)).collect::<Option<Vec<_>>>())
        .ok_or_else(|| format!("{} : sandbox.{} doit etre une liste de chaines", server_name, key))
}

enum ModeSetting {
    Off,
    On(SandboxMode),
}

/// Une valeur inconnue est une erreur : la traiter comme « pas d'isolation » retirerait la
/// protection en silence pour une simple faute de frappe.
fn mode_of(server_name: &str, s: &str) -> Result<ModeSetting, String> {
    match s.to_ascii_lowercase().as_str() {
        "shield" | "bouclier" | "on" | "true" => Ok(ModeSetting::On(SandboxMode::Shield)),
        "strict" => Ok(ModeSetting::On(SandboxMode::Strict)),
        "off" | "false" | "none" => Ok(ModeSetting::Off),
        _ => Err(format!(
            "{} : mode sandbox inconnu \"{}\" (shield, strict ou false)",
            server_name, s
        )),
    }
}

const SPEC_KEYS: &[&str] = &["mode", "network", "allowRead", "allowWrite", "allowExec"];

/// Réglage effectif d'un serveur : `mcpServers.X.sandbox` (false / true / "shield" / "strict" /
/// objet), sinon `settings.sandbox` global, sinon pas d'isolation.
pub fn resolve(
    server_name: &str,
    server: &Value,
    global: Option<&Value>,
    command: &str,
    args: &[String],
) -> Result<Option<SandboxSpec>, String> {
    let local = server.get("sandbox");
    let explicit = local.is_some();
    let (spec_value, label) = match local {
        Some(Value::Bool(false)) => return Ok(None),
        Some(v) => (v.clone(), server_name.to_string()),
        None => match global {
            Some(Value::Bool(false)) | None => return Ok(None),
            Some(g) => (g.clone(), "settings.sandbox".to_string()),
        },
    };
    let mut spec = SandboxSpec {
        mode: SandboxMode::Shield,
        network: true,
        allow_read: Vec::new(),
        allow_write: Vec::new(),
        allow_exec: Vec::new(),
    };
    match &spec_value {
        Value::Bool(true) => {}
        Value::String(s) => match mode_of(&label, s)? {
            ModeSetting::Off => return Ok(None),
            ModeSetting::On(mode) => spec.mode = mode,
        },
        Value::Object(o) => {
            if let Some(unknown) = o.keys().find(|k| !SPEC_KEYS.contains(&k.as_str())) {
                return Err(format!(
                    "{} : cle sandbox inconnue \"{}\" ({})",
                    label,
                    unknown,
                    SPEC_KEYS.join(", ")
                ));
            }
            match o.get("mode") {
                None => {}
                Some(Value::String(m)) => match mode_of(&label, m)? {
                    ModeSetting::Off => return Ok(None),
                    ModeSetting::On(mode) => spec.mode = mode,
                },
                Some(_) => return Err(format!("{} : sandbox.mode doit etre une chaine", label)),
            }
            match o.get("network") {
                None => {}
                Some(Value::Bool(n)) => spec.network = *n,
                Some(_) => return Err(format!("{} : sandbox.network doit etre true ou false", label)),
            }
            spec.allow_read = str_list(&label, "allowRead", o.get("allowRead"))?;
            spec.allow_write = str_list(&label, "allowWrite", o.get("allowWrite"))?;
            spec.allow_exec = str_list(&label, "allowExec", o.get("allowExec"))?;
        }
        _ => {
            return Err(format!(
                "{} : sandbox doit etre true, false, \"shield\", \"strict\" ou un objet",
                label
            ))
        }
    }
    if !explicit {
        let haystack = format!("{} {} {}", server_name, command, args.join(" ")).to_lowercase();
        if UNSANDBOXED_BY_DEFAULT.iter().any(|n| haystack.contains(n)) {
            return Ok(None);
        }
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
    Ok(Some(spec))
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
    denied.extend(crate::instance::sandbox_hub_rules(home));
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

// ─── Linux : bubblewrap ──────────────────────────────────────
//
// Même politique qu'avec sandbox-exec, en espaces de noms : la machine reste visible, mais chaque
// dossier secret est remplacé par un dossier vide en mémoire (tmpfs) et chaque fichier secret par
// /dev/null. Les écritures dans ces dossiers vides disparaissent avec le serveur. En mode strict,
// tout est en lecture seule sauf les dossiers temporaires, les caches et `allowWrite`.

/// bwrap utilisable (installé et autorisé par le noyau à créer un espace de noms), testé une fois.
fn bwrap_path() -> Option<String> {
    static FOUND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let exe = ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"]
                .into_iter()
                .find(|p| Path::new(p).exists())?;
            let ok = std::process::Command::new(exe)
                .args(["--dev-bind", "/", "/", "--", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            ok.then(|| exe.to_string())
        })
        .clone()
}

fn empty_file() -> String {
    let p = std::env::temp_dir().join(format!("mcphub-empty-{}", std::process::id()));
    let ok = std::fs::metadata(&p).map(|m| m.is_file() && m.len() == 0).unwrap_or(false)
        || std::fs::write(&p, b"").is_ok();
    #[cfg(unix)]
    if ok {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444));
    }
    if ok {
        p.to_string_lossy().to_string()
    } else {
        "/dev/null".into()
    }
}

/// Dossiers de McpHub à cacher : ~/.McpHub, toutes les instances ~/.McpHub-*, l'instance courante.
fn hub_dirs(home: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.eq_ignore_ascii_case(".mcphub") || n.to_ascii_lowercase().starts_with(".mcphub-") {
                out.push(format!("{}/{}", home, n));
            }
        }
    }
    let own = crate::instance::hub_dir().to_string_lossy().to_string();
    if !out.contains(&own) {
        out.push(own);
    }
    out
}

pub fn bwrap_args(spec: &SandboxSpec, home: &str, referenced: &[String]) -> Vec<String> {
    let mut a: Vec<String> = vec!["--die-with-parent".into(), "--new-session".into()];
    let push = |a: &mut Vec<String>, parts: &[&str]| a.extend(parts.iter().map(|p| p.to_string()));
    if spec.mode == SandboxMode::Strict {
        push(&mut a, &["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
        let mut writable: Vec<String> = vec!["/tmp".into(), "/var/tmp".into(), "/dev/shm".into()];
        for d in [".npm", ".cache", ".bun", ".pnpm-store", ".local/share/pnpm"] {
            let p = format!("{}/{}", home, d);
            let _ = std::fs::create_dir_all(&p);
            writable.push(p);
        }
        for w in spec.allow_write.iter().map(|w| expand(w, home)) {
            if w.starts_with('/') && !is_ancestor_of_secret(&w, home) {
                writable.push(w);
            }
        }
        for w in writable {
            if Path::new(&w).exists() {
                push(&mut a, &["--bind", &w, &w]);
            }
        }
    } else {
        push(&mut a, &["--dev-bind", "/", "/"]);
    }
    // Secrets : dossiers vides, fichiers remplacés par /dev/null.
    let mut hidden: Vec<String> = SECRET_DIRS.iter().map(|d| format!("{}/{}", home, d)).collect();
    hidden.extend(hub_dirs(home));
    // Fichier vide en lecture seule plutôt que /dev/null (inutilisable sur un montage nodev) :
    // un shell lit un ~/.bashrc vide au lieu d'afficher « Permission denied ».
    let empty = empty_file();
    for d in hidden {
        match std::fs::metadata(&d) {
            Ok(m) if m.is_dir() => push(&mut a, &["--tmpfs", &d]),
            Ok(_) => push(&mut a, &["--ro-bind", &empty, &d]),
            Err(_) => {}
        }
    }
    for f in SECRET_FILES {
        let p = format!("{}/{}", home, f);
        if Path::new(&p).is_file() {
            push(&mut a, &["--ro-bind", &empty, &p]);
        }
    }
    for f in WRITE_PROTECTED {
        let p = format!("{}/{}", home, f);
        if Path::new(&p).exists() {
            push(&mut a, &["--ro-bind", &p, &p]);
        }
    }
    // Exceptions en lecture seule : chemins précis à l'intérieur d'un secret, que le serveur
    // référence lui-même (une clé .p8 passée en variable) ou autorisés par `allowRead`.
    let reads: Vec<String> = referenced
        .iter()
        .cloned()
        .chain(spec.allow_read.iter().map(|r| expand(r, home)))
        .filter(|r| r.starts_with('/') && strictly_inside_secret(r, home) && Path::new(r).exists())
        .collect();
    for r in reads {
        push(&mut a, &["--ro-bind", &r, &r]);
    }
    // Un dossier secret explicitement autorisé en écriture (ex. ~/.mcp-auth) reste réel.
    for w in spec.allow_write.iter().map(|w| expand(w, home)) {
        if SECRET_DIRS.iter().any(|d| w == format!("{}/{}", home, d)) && Path::new(&w).is_dir() {
            push(&mut a, &["--bind", &w, &w]);
        }
    }
    if !spec.network {
        a.push("--unshare-net".into());
    }
    a
}

/// Commande enveloppée : `/usr/bin/sandbox-exec -p PROFIL commande args...` (macOS) ou
/// `bwrap ... -- commande args...` (Linux). Sans l'un ni l'autre, la commande est lancée telle
/// quelle avec un avertissement.
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
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    if cfg!(target_os = "linux") {
        if let Some(bwrap) = bwrap_path() {
            let referenced = referenced_paths(env, args, &home);
            let mut new_args = bwrap_args(spec, &home, &referenced);
            new_args.push("--".into());
            new_args.push(command.to_string());
            new_args.extend(args.iter().cloned());
            return (bwrap, new_args);
        }
        eprintln!("[McpHub][SANDBOX] {} : bubblewrap (bwrap) indisponible ou refuse par le noyau, serveur lance sans isolation (apt install bubblewrap)", server);
        return (command.to_string(), args.to_vec());
    }
    let exe = "/usr/bin/sandbox-exec";
    if !cfg!(target_os = "macos") || !Path::new(exe).exists() {
        eprintln!("[McpHub][SANDBOX] {} : sandbox-exec indisponible, serveur lance sans isolation", server);
        return (command.to_string(), args.to_vec());
    }
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
        let ok = |name: &str, server: Value, global: Option<&Value>, args: &[String]| {
            resolve(name, &server, global, "npx", args).expect("reglage valide")
        };
        assert!(ok("supabase", json!({}), Some(&g), &[]).is_some());
        assert!(ok("supabase", json!({"sandbox": false}), Some(&g), &[]).is_none());
        assert!(ok("supabase", json!({"sandbox": "off"}), Some(&g), &[]).is_none());
        assert!(ok("supabase", json!({}), None, &[]).is_none());
        assert!(ok("dc", json!({}), Some(&g), &["@wonderwhy-er/desktop-commander".into()]).is_none());
        assert!(ok("dc", json!({"sandbox": "shield"}), Some(&g), &["desktop-commander".into()]).is_some());
        let s = resolve("x", &json!({"sandbox": {"mode": "strict", "network": false, "allowWrite": ["~/data"]}}), None, "xurl", &[])
            .unwrap()
            .unwrap();
        assert_eq!(s.mode, SandboxMode::Strict);
        assert!(!s.network);
        assert!(s.allow_write.contains(&"~/data".to_string()));
        assert!(s.allow_write.contains(&"~/.xurl".to_string()));
        let r = ok("cf", json!({}), Some(&g), &["-y".into(), "mcp-remote@0.15.1".into()]).unwrap();
        assert!(r.allow_write.contains(&"~/.mcp-auth".to_string()));
    }

    #[test]
    fn faute_de_frappe_refusee_au_lieu_de_retirer_l_isolation() {
        assert!(resolve("a", &json!({"sandbox": "sheild"}), None, "npx", &[]).is_err());
        assert!(resolve("a", &json!({}), Some(&json!("stirct")), "npx", &[]).is_err());
        assert!(resolve("a", &json!({"sandbox": {"mode": "shield", "allowWrites": ["~/x"]}}), None, "npx", &[]).is_err());
        assert!(resolve("a", &json!({"sandbox": {"network": "no"}}), None, "npx", &[]).is_err());
        assert!(resolve("a", &json!({"sandbox": 1}), None, "npx", &[]).is_err());
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
    fn bubblewrap_bouclier_et_strict() {
        let home = std::env::temp_dir().join(format!("mcphub-bw-{}", std::process::id()));
        let h = home.to_string_lossy().to_string();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".mcp-auth")).unwrap();
        std::fs::write(home.join(".netrc"), "x").unwrap();
        std::fs::write(home.join(".ssh/deploy.p8"), "k").unwrap();
        let shield = SandboxSpec { mode: SandboxMode::Shield, network: false, allow_read: vec![], allow_write: vec!["~/.mcp-auth".into()], allow_exec: vec![] };
        let key = format!("{}/.ssh/deploy.p8", h);
        let a = bwrap_args(&shield, &h, &[key.clone()]).join(" ");
        assert!(a.starts_with("--die-with-parent --new-session --dev-bind / /"), "{}", a);
        assert!(a.contains(&format!("--tmpfs {}/.ssh", h)));
        assert!(a.contains(&format!("{}/.netrc", h)) && a.contains("mcphub-empty-"));
        let tmpfs_at = a.find(&format!("--tmpfs {}/.ssh", h)).unwrap();
        let key_at = a.find(&format!("--ro-bind {} {}", key, key)).unwrap();
        assert!(key_at > tmpfs_at, "l'exception suit le masquage");
        assert!(a.contains(&format!("--bind {0}/.mcp-auth {0}/.mcp-auth", h)));
        assert!(a.ends_with("--unshare-net"));
        let strict = SandboxSpec { mode: SandboxMode::Strict, network: true, allow_read: vec![], allow_write: vec!["~".into()], allow_exec: vec![] };
        let b = bwrap_args(&strict, &h, &[]).join(" ");
        assert!(b.contains("--ro-bind / / --dev /dev --proc /proc"));
        assert!(b.contains("--bind /tmp /tmp"));
        assert!(!b.contains(&format!("--bind {0} {0} ", h)), "~ ne doit jamais etre rouvert");
        assert!(!b.contains("--unshare-net"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn guillemets_echappes() {
        assert_eq!(q("/a \"b\""), "\"/a \\\"b\\\"\"");
    }
}
