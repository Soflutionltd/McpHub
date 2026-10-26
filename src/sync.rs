//! Synchronisation entre machines (Mac, VPS Linux...) sans aucun secret en clair.
//!
//! `McpHub sync export FICHIER` écrit la config des serveurs où chaque secret est une référence
//! `${NOM}` : les valeurs littérales qui ressemblent à des secrets (variables *_TOKEN, *_KEY,
//! en-têtes Authorization, options --api-key...) sont remplacées par une référence, et les chemins
//! du dossier personnel deviennent `${HOME}/...` pour fonctionner sur l'autre machine.
//!
//! Avec `--secrets`, les valeurs voyagent aussi, chiffrées par une phrase de passe :
//! scrypt (N = 2^15, r = 8, p = 1) puis ChaCha20-Poly1305. Sans la phrase, le fichier ne révèle
//! que les noms des secrets. `McpHub sync import FICHIER [--secrets]` fusionne les serveurs et
//! range les secrets déchiffrés au Trousseau (ou dans secrets.env, mode 600, sous Linux).
//! Un serveur dont le programme manque sur la machine d'arrivée est importé désactivé.

use std::collections::BTreeMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde_json::{json, Map, Value};

const FORMAT: &str = "mcphub-sync/1";
const SCRYPT_LOG_N: u8 = 15;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("hexadecimal invalide".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "hexadecimal invalide".to_string()))
        .collect()
}

fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("source aleatoire indisponible : {}", e))?;
    Ok(buf)
}

fn derive_key(passphrase: &str, salt: &[u8], log_n: u8) -> Result<[u8; 32], String> {
    let params = scrypt::Params::new(log_n, 8, 1, 32).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut key).map_err(|e| e.to_string())?;
    Ok(key)
}

pub fn encrypt(plain: &[u8], passphrase: &str) -> Result<Value, String> {
    let salt = random_bytes(16)?;
    let nonce = random_bytes(12)?;
    let key = derive_key(passphrase, &salt, SCRYPT_LOG_N)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: FORMAT.as_bytes() })
        .map_err(|_| "chiffrement impossible".to_string())?;
    Ok(json!({
        "cipher": "chacha20-poly1305",
        "kdf": {"name": "scrypt", "logN": SCRYPT_LOG_N, "r": 8, "p": 1, "salt": hex(&salt)},
        "nonce": hex(&nonce),
        "data": hex(&ct),
    }))
}

pub fn decrypt(box_: &Value, passphrase: &str) -> Result<Vec<u8>, String> {
    let salt = unhex(box_.pointer("/kdf/salt").and_then(|v| v.as_str()).unwrap_or(""))?;
    let log_n = box_.pointer("/kdf/logN").and_then(|v| v.as_u64()).unwrap_or(SCRYPT_LOG_N as u64) as u8;
    if !(10..=20).contains(&log_n) {
        return Err("parametres de chiffrement invalides".into());
    }
    let nonce = unhex(box_.get("nonce").and_then(|v| v.as_str()).unwrap_or(""))?;
    let data = unhex(box_.get("data").and_then(|v| v.as_str()).unwrap_or(""))?;
    if nonce.len() != 12 {
        return Err("parametres de chiffrement invalides".into());
    }
    let key = derive_key(passphrase, &salt, log_n)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &data, aad: FORMAT.as_bytes() })
        .map_err(|_| "phrase de passe incorrecte ou fichier modifie".to_string())
}

// ─── Nettoyage de la config ──────────────────────────────────

const SECRET_WORDS: &[&str] = &[
    "TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "ACCESS_KEY", "PRIVATE_KEY",
    "CLIENT_SECRET", "AUTH", "CREDENTIAL", "COOKIE", "SESSION", "_KEY", "_PAT", "DSN",
];

fn looks_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase().replace('-', "_");
    SECRET_WORDS.iter().any(|w| up.contains(w)) || up.ends_with("KEY")
}

fn var_name(server: &str, label: &str) -> String {
    let raw = format!("{}_{}", server, label.trim_start_matches('-'));
    let mut out: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.chars().next().map_or(true, |c| c.is_ascii_digit()) {
        format!("MCP_{}", out)
    } else {
        out
    }
}

/// Noms ${NOM} cités dans une chaîne.
fn refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                out.push(after[..j].to_string());
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

struct Cleaner {
    home: String,
    /// Secrets à emporter : nom -> valeur (littérales retirées de la config).
    moved: BTreeMap<String, String>,
    /// Références ${NOM} rencontrées.
    referenced: Vec<String>,
}

impl Cleaner {
    fn portable(&self, s: &str) -> String {
        if self.home.len() > 1 && s.contains(&self.home) {
            s.replace(&self.home, "${HOME}")
        } else {
            s.to_string()
        }
    }

    fn note_refs(&mut self, s: &str) {
        for r in refs(s) {
            if r != "HOME" && !self.referenced.contains(&r) {
                self.referenced.push(r);
            }
        }
    }

    /// Valeur littérale secrète -> ${NOM}, valeur gardée à part.
    fn take(&mut self, var: String, value: &str) -> String {
        let var = if self.moved.get(&var).map_or(false, |v| v != value) {
            format!("{}_{}", var, self.moved.len() + 1)
        } else {
            var
        };
        self.moved.insert(var.clone(), value.to_string());
        self.referenced.push(var.clone());
        format!("${{{}}}", var)
    }

    fn server(&mut self, name: &str, srv: &mut Value) {
        if let Some(c) = srv.get("command").and_then(|v| v.as_str()).map(String::from) {
            srv["command"] = json!(self.portable(&c));
        }
        if let Some(env) = srv.get_mut("env").and_then(|v| v.as_object_mut()) {
            let keys: Vec<String> = env.keys().cloned().collect();
            for k in keys {
                let Some(v) = env.get(&k).and_then(|v| v.as_str()).map(String::from) else { continue };
                let new = if v.contains("${") || v.is_empty() || !looks_secret_name(&k) {
                    self.portable(&v)
                } else {
                    self.take(k.clone(), &v)
                };
                self.note_refs(&new);
                env.insert(k, json!(new));
            }
        }
        if let Some(h) = srv.get_mut("headers").and_then(|v| v.as_object_mut()) {
            let keys: Vec<String> = h.keys().cloned().collect();
            for k in keys {
                let Some(v) = h.get(&k).and_then(|v| v.as_str()).map(String::from) else { continue };
                let secret = k.eq_ignore_ascii_case("authorization") || looks_secret_name(&k);
                let new = if v.contains("${") || !secret {
                    v.clone()
                } else if let Some(token) = v.strip_prefix("Bearer ") {
                    format!("Bearer {}", self.take(var_name(name, "token"), token))
                } else {
                    self.take(var_name(name, &k), &v)
                };
                self.note_refs(&new);
                h.insert(k, json!(new));
            }
        }
        if let Some(args) = srv.get_mut("args").and_then(|v| v.as_array_mut()) {
            let mut secret_next: Option<String> = None;
            for a in args.iter_mut() {
                let Some(v) = a.as_str().map(String::from) else { continue };
                let new = if let Some(flag) = secret_next.take() {
                    if v.contains("${") { v.clone() } else { self.take(var_name(name, &flag), &v) }
                } else if let Some((flag, val)) = v.split_once('=').filter(|(f, val)| f.starts_with("--") && looks_secret_name(f) && !val.contains("${")) {
                    format!("{}={}", flag, self.take(var_name(name, flag), val))
                } else {
                    if v.starts_with("--") && !v.contains('=') && looks_secret_name(&v) {
                        secret_next = Some(v.clone());
                    }
                    self.portable(&v)
                };
                self.note_refs(&new);
                *a = json!(new);
            }
        }
    }
}

/// Copie de la config prête à voyager, et les secrets retirés (nom -> valeur).
pub fn portable_config(config: &Value, home: &str) -> (Value, BTreeMap<String, String>, Vec<String>) {
    let mut out = config.clone();
    let mut c = Cleaner { home: home.to_string(), moved: BTreeMap::new(), referenced: Vec::new() };
    for key in ["mcpServers", "servers"] {
        if let Some(servers) = out.get_mut(key).and_then(|v| v.as_object_mut()) {
            for (name, srv) in servers.iter_mut() {
                c.server(name, srv);
            }
        }
    }
    let mut referenced = c.referenced;
    referenced.sort();
    referenced.dedup();
    (out, c.moved, referenced)
}

// ─── Commandes ───────────────────────────────────────────────

fn passphrase(confirm: bool) -> Result<String, String> {
    if let Ok(p) = std::env::var("MCPHUB_SYNC_PASSPHRASE") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let p = crate::vault::read_secret_from_stdin("phrase de passe de synchronisation")?;
    if p.chars().count() < 12 {
        return Err("phrase de passe trop courte (12 caracteres minimum)".into());
    }
    if confirm {
        let again = crate::vault::read_secret_from_stdin("la meme phrase, pour confirmer")?;
        if again != p {
            return Err("les deux saisies different".into());
        }
    }
    Ok(p)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "machine".into())
}

pub fn export(file: &str, with_secrets: bool) -> Result<String, String> {
    let path = crate::instance::hub_dir().join("config.json");
    let raw = std::fs::read_to_string(&path).map_err(|_| format!("{} introuvable", path.display()))?;
    let config: Value = serde_json::from_str(&raw).map_err(|e| format!("config.json invalide : {}", e))?;
    let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
    let (portable, moved, referenced) = portable_config(&config, &home);
    let mut bundle = json!({
        "format": FORMAT,
        "exportedAt": crate::audit::iso_utc_now(),
        "from": hostname(),
        "mcphub": env!("CARGO_PKG_VERSION"),
        "config": portable,
        "secretNames": referenced,
    });
    let mut missing = Vec::new();
    if with_secrets {
        let mut values: Map<String, Value> = Map::new();
        for name in &referenced {
            let v = moved
                .get(name)
                .cloned()
                .or_else(|| crate::vault::get(name).ok().flatten())
                .or_else(|| std::env::var(name).ok());
            match v {
                Some(v) => {
                    values.insert(name.clone(), json!(v));
                }
                None => missing.push(name.clone()),
            }
        }
        let pass = passphrase(true)?;
        bundle["secrets"] = encrypt(Value::Object(values).to_string().as_bytes(), &pass)?;
    }
    let text = serde_json::to_string_pretty(&bundle).map_err(|e| e.to_string())?;
    if file == "-" {
        println!("{}", text);
    } else {
        std::fs::write(file, text).map_err(|e| e.to_string())?;
        crate::vault::restrict_permissions(std::path::Path::new(file));
    }
    let mut msg = format!(
        "{} serveurs exportes, {} secrets {}",
        bundle.pointer("/config/mcpServers").or_else(|| bundle.pointer("/config/servers")).and_then(|v| v.as_object()).map_or(0, |o| o.len()),
        referenced.len(),
        if with_secrets { "chiffres" } else { "cites par leur nom seulement (ajouter --secrets pour les emporter chiffres)" }
    );
    if !moved.is_empty() {
        msg.push_str(&format!(". Valeurs en clair retirees de la copie : {}", moved.keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    if !missing.is_empty() {
        msg.push_str(&format!(". Introuvables ici : {}", missing.join(", ")));
    }
    Ok(msg)
}

fn command_available(cmd: &str) -> bool {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
    let cmd = cmd.replace("${HOME}", &home);
    crate::registry::find_cmd(&cmd).is_some()
}

pub fn import(file: &str, with_secrets: bool, with_settings: bool) -> Result<String, String> {
    let raw = std::fs::read_to_string(file).map_err(|e| format!("{} : {}", file, e))?;
    let bundle: Value = serde_json::from_str(&raw).map_err(|e| format!("fichier invalide : {}", e))?;
    if bundle.get("format").and_then(|v| v.as_str()) != Some(FORMAT) {
        return Err("ce fichier n'est pas un export McpHub sync".into());
    }
    let incoming = bundle.get("config").cloned().unwrap_or_else(|| json!({}));
    let path = crate::instance::hub_dir().join("config.json");
    let mut local: Value = match std::fs::read_to_string(&path) {
        Ok(r) => serde_json::from_str(&r).map_err(|e| format!("config.json local invalide : {}", e))?,
        Err(_) => json!({"mcpServers": {}}),
    };
    let lkey = if local.get("servers").and_then(|v| v.as_object()).is_some() { "servers" } else { "mcpServers" };
    if local.get(lkey).and_then(|v| v.as_object()).is_none() {
        local[lkey] = json!({});
    }
    let mut added = 0;
    let mut disabled = Vec::new();
    let servers = incoming.get("mcpServers").or_else(|| incoming.get("servers")).and_then(|v| v.as_object()).cloned().unwrap_or_default();
    for (name, mut srv) in servers {
        if let Some(cmd) = srv.get("command").and_then(|v| v.as_str()).map(String::from) {
            if !command_available(&cmd) && srv.get("disabled").and_then(|v| v.as_bool()) != Some(true) {
                srv["disabled"] = json!(true);
                disabled.push(format!("{} ({} absent)", name, cmd));
            }
        }
        local[lkey][&name] = srv;
        added += 1;
    }
    if with_settings {
        if let Some(s) = incoming.get("settings") {
            local["settings"] = s.clone();
        }
    }
    let mut stored = 0;
    if with_secrets {
        let box_ = bundle.get("secrets").ok_or("ce fichier ne contient pas de secrets (export sans --secrets)")?;
        let pass = passphrase(false)?;
        let plain = decrypt(box_, &pass)?;
        let values: Map<String, Value> = serde_json::from_slice(&plain).map_err(|_| "contenu chiffre illisible".to_string())?;
        for (k, v) in values {
            if let Some(v) = v.as_str() {
                if crate::vault::get(&k).ok().flatten().as_deref() != Some(v) {
                    crate::vault::set(&k, v).map_err(|e| format!("{} : {}", k, e))?;
                }
                stored += 1;
            }
        }
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&tmp, serde_json::to_string_pretty(&local).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    crate::vault::restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    let names: Vec<String> = bundle.get("secretNames").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let absent: Vec<String> = names.iter().filter(|n| std::env::var(n).is_err() && crate::vault::get(n).ok().flatten().is_none()).cloned().collect();
    let mut msg = format!("{} serveurs importes depuis {}", added, bundle.get("from").and_then(|v| v.as_str()).unwrap_or("?"));
    if with_secrets {
        msg.push_str(&format!(", {} secrets ranges au coffre", stored));
    }
    if !disabled.is_empty() {
        msg.push_str(&format!(". Desactives (programme absent ici) : {}", disabled.join(", ")));
    }
    if !absent.is_empty() {
        msg.push_str(&format!(". Secrets encore manquants : {} (McpHub vault set NOM)", absent.join(", ")));
    }
    Ok(msg)
}

pub fn run_cli(args: &[String]) -> i32 {
    let flags: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let with_secrets = flags.contains(&"--secrets");
    let file = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned();
    let res = match (flags.first().copied(), file) {
        (Some("export"), f) => export(&f.unwrap_or_else(|| "-".into()), with_secrets),
        (Some("import"), Some(f)) => import(&f, with_secrets, flags.contains(&"--settings")),
        _ => {
            println!(
                "McpHub sync : la config d'une machine a l'autre, sans secret en clair\n\n  \
                 export FICHIER [--secrets]              config portable (${{NOM}} a la place des secrets, ${{HOME}} pour les chemins)\n  \
                 import FICHIER [--secrets] [--settings] fusion des serveurs, secrets au coffre\n\n\
                 --secrets : valeurs chiffrees (scrypt + ChaCha20-Poly1305) par une phrase de passe,\n            \
                 demandee au clavier ou lue dans MCPHUB_SYNC_PASSPHRASE."
            );
            return 0;
        }
    };
    match res {
        Ok(msg) => {
            eprintln!("{}", msg);
            0
        }
        Err(e) => {
            eprintln!("McpHub sync : {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chiffrement_aller_retour() {
        let b = encrypt(b"{\"A\":\"1\"}", "une phrase assez longue").unwrap();
        assert_eq!(decrypt(&b, "une phrase assez longue").unwrap(), b"{\"A\":\"1\"}");
        assert!(decrypt(&b, "mauvaise phrase de passe").is_err());
        let mut altered = b.clone();
        let d = altered["data"].as_str().unwrap().to_string();
        let flipped = if d.ends_with('0') { format!("{}1", &d[..d.len() - 1]) } else { format!("{}0", &d[..d.len() - 1]) };
        altered["data"] = json!(flipped);
        assert!(decrypt(&altered, "une phrase assez longue").is_err());
        assert!(!b.to_string().contains("\"1\""));
    }

    #[test]
    fn config_portable_sans_secret() {
        let cfg = json!({"mcpServers": {
            "gh": {"command": "/Users/a/.local/bin/gh-mcp", "args": ["--token", "ghp_x", "--dir=/Users/a/code", "--api-key=k1"],
                   "env": {"GITHUB_TOKEN": "ghp_x", "MODE": "fast", "REF": "${OTHER}"}},
            "remote": {"url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer abc", "X-Trace": "1"}}
        }});
        let (out, moved, refs) = portable_config(&cfg, "/Users/a");
        let txt = out.to_string();
        assert!(!txt.contains("ghp_x") && !txt.contains("abc") && !txt.contains("k1"), "{}", txt);
        assert_eq!(out["mcpServers"]["gh"]["command"], "${HOME}/.local/bin/gh-mcp");
        assert_eq!(out["mcpServers"]["gh"]["args"][2], "--dir=${HOME}/code");
        assert_eq!(out["mcpServers"]["gh"]["env"]["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
        assert_eq!(out["mcpServers"]["gh"]["env"]["MODE"], "fast");
        assert_eq!(out["mcpServers"]["remote"]["headers"]["Authorization"], "Bearer ${REMOTE_TOKEN}");
        assert_eq!(out["mcpServers"]["remote"]["headers"]["X-Trace"], "1");
        assert_eq!(moved.get("GITHUB_TOKEN").map(|s| s.as_str()), Some("ghp_x"));
        assert!(refs.contains(&"OTHER".to_string()) && refs.contains(&"REMOTE_TOKEN".to_string()));
    }
}
