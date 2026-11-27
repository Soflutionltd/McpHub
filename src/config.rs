use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::protocol::MediaProfile;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub pool: usize,
    pub request_timeout_secs: Option<u64>,
    /// Isolation macOS (sandbox-exec), None = sans isolation.
    pub sandbox: Option<crate::sandbox::SandboxSpec>,
    /// Variables du démon explicitement transmises à ce serveur (`inheritEnv`). Par défaut un
    /// serveur ne reçoit que l'environnement système de base et son propre `env`.
    pub inherit_env: Vec<String>,
    /// Serveur distant (Streamable HTTP natif) au lieu d'une commande.
    pub url: Option<String>,
    /// En-têtes HTTP fixes du serveur distant (${VAR} résolus).
    pub headers: HashMap<String, String>,
    /// OAuth géré par McpHub (jetons au Trousseau).
    pub oauth: bool,
    /// Instance d'un tenant (`serveur@tenant`) : environnement en liste blanche, HOME isolé,
    /// jamais l'agent SSH ni les variables du démon.
    pub tenant: Option<String>,
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
    /// Les serveurs les plus appelés d'après les statistiques d'usage (`preload_auto_count`).
    Auto,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub servers: HashMap<String, ServerConfig>,
    pub mode: Mode,
    pub preload: Preload,
    /// Nombre de serveurs préchauffés par `preload: "auto"`.
    pub preload_auto_count: usize,
    /// Les serveurs préchauffés ne sont jamais arrêtés pour inactivité (`keepPreloadedWarm`).
    pub keep_preloaded_warm: bool,
    pub media_profile: MediaProfile,
    pub idle_timeout_ms: u64,
    pub preload_delay_ms: u64,
    pub request_timeout_secs: u64,
    /// Au-delà, `mcphub_call` répond « toujours en cours » avec un numéro et l'appel continue
    /// (`settings.deferAfterSecs`, 0 = jamais).
    pub defer_after_secs: u64,
    pub health_check_interval_secs: u64,
    pub health_auto_restart: bool,
    pub health_notifications: bool,
    pub permissions: crate::permissions::PermissionsConfig,
    pub audit: crate::audit::AuditConfig,
    /// Épinglage, détection d'empoisonnement, filtres d'outils et profils.
    pub guard: crate::toolguard::GuardConfig,
    /// Code mode (`mcphub_run`).
    pub code_mode: crate::codemode::CodeModeConfig,
    /// Mise à jour automatique depuis les releases signées (`settings.autoUpdate`).
    pub auto_update: bool,
    /// Cache des lectures (`settings.responseCache`).
    pub response_cache: crate::response_cache::CacheSettings,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
            mode: Mode::Discover,
            // Rien n'est préchauffé sans réglage explicite : `settings.preload` n'était lu nulle
            // part, l'activer par défaut lancerait tous les serveurs à chaque démarrage.
            preload: Preload::None,
            preload_auto_count: 5,
            keep_preloaded_warm: true,
            media_profile: MediaProfile::Safe,
            idle_timeout_ms: 5 * 60 * 1000,
            preload_delay_ms: 200,
            request_timeout_secs: 120,
            defer_after_secs: 45,
            health_check_interval_secs: 30,
            health_auto_restart: true,
            health_notifications: true,
            permissions: crate::permissions::PermissionsConfig::default(),
            audit: crate::audit::AuditConfig::default(),
            guard: crate::toolguard::GuardConfig::default(),
            code_mode: crate::codemode::CodeModeConfig::default(),
            auto_update: true,
            response_cache: crate::response_cache::CacheSettings::default(),
        }
    }
}

/// Chemin qui désigne McpHub lui-même (binaire, client Node...). Les serveurs installés par la
/// bibliothèque vivent dans .../McpHub/servers/ : leur chemin contient « McpHub » sans l'être.
fn names_mcphub(s: &str) -> bool {
    let lower = s.to_lowercase();
    let bundles = crate::registry::servers_dir()
        .to_string_lossy()
        .to_lowercase();
    let lower = match lower.strip_prefix(&bundles) {
        Some(rest) => rest.to_string(),
        None => lower,
    };
    lower.contains("mcphub") || lower.contains("mcp-on-demand")
}

/// Un argument désigne McpHub quand un élément de son chemin est McpHub (`mcphub`,
/// `McpHub/client/index.js`, `mcp-hub`…), jamais quand il cite le mot ailleurs : une valeur
/// comme `com.soflution.mcphub/roots` ne fait pas d'un serveur une copie de McpHub.
fn arg_names_mcphub(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    let bundles = crate::registry::servers_dir()
        .to_string_lossy()
        .to_lowercase();
    let lower = lower.strip_prefix(&bundles).unwrap_or(&lower);
    lower.split('/').any(|part| {
        ["mcphub", "mcp-hub", "mcp-on-demand"]
            .iter()
            .any(|name| part.starts_with(name))
    })
}

fn is_self(name: &str, config: &Value) -> bool {
    let lower = name.to_lowercase();
    if lower == "mcphub" || lower == "mcp-hub" || lower == "mcp-on-demand" {
        return true;
    }
    if let Some(cmd) = config.get("command").and_then(|v| v.as_str()) {
        if names_mcphub(cmd) {
            return true;
        }
    }
    if let Some(args) = config.get("args").and_then(|v| v.as_array()) {
        if args
            .iter()
            .any(|a| a.as_str().map(arg_names_mcphub).unwrap_or(false))
        {
            return true;
        }
    }
    false
}

static SECRETS_FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Charge dans l'environnement du process les secrets qui n'y sont pas encore : au démarrage, puis
/// à chaque rechargement de la config (un secret ajouté depuis la bibliothèque ou `McpHub vault
/// set` sert sans redémarrer). Ordre de priorité : environnement réel > Trousseau macOS (vault) >
/// ~/.McpHub/secrets.env. Les variables déjà présentes ne sont jamais écrasées.
fn load_secrets_env() {
    let first = SECRETS_FIRST.swap(false, std::sync::atomic::Ordering::SeqCst);
    {
        let (from_vault, missing) = crate::vault::load_into_env();
        if from_vault > 0 || (first && !missing.is_empty()) {
            eprintln!(
                "[McpHub][INFO] Trousseau: {} secrets charges{}",
                from_vault,
                if missing.is_empty() {
                    String::new()
                } else {
                    format!(", introuvables: {}", missing.join(", "))
                }
            );
        }
        // Même fichier que le coffre hors macOS (~/.McpHub/secrets.env, ou le dossier de l'instance).
        let path = match crate::vault::secrets_env_path() {
            Some(p) => p,
            None => return,
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut n = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let mut v = v.trim();
                if v.len() >= 2
                    && ((v.starts_with('"') && v.ends_with('"'))
                        || (v.starts_with('\'') && v.ends_with('\'')))
                {
                    v = &v[1..v.len() - 1];
                }
                if !k.is_empty() && std::env::var(k).is_err() {
                    std::env::set_var(k, v);
                    n += 1;
                }
            }
        }
        if first || n > 0 {
            eprintln!("[McpHub][INFO] secrets.env charge ({} variables)", n);
        }
    }
}

/// Remplace les motifs ${VAR} par la valeur de la variable d'environnement.
/// Si la variable est absente, le motif est laisse tel quel et un avertissement est emis.
fn expand_vars(input: &str) -> String {
    expand_with(input, &mut |var| match std::env::var(var) {
        Ok(val) => Some(val),
        Err(_) => {
            eprintln!(
                "[McpHub][WARN] variable ${{{}}} introuvable (Trousseau et secrets.env)",
                var
            );
            None
        }
    })
}

/// Remplace chaque ${VAR} par `lookup(VAR)` ; un motif sans valeur est laissé tel quel.
pub(crate) fn expand_with(input: &str, lookup: &mut dyn FnMut(&str) -> Option<String>) -> String {
    if !input.contains("${") {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var = &after[..end];
            match lookup(var) {
                Some(val) => out.push_str(&val),
                None => {
                    out.push_str("${");
                    out.push_str(var);
                    out.push('}');
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Fonction de remplacement des ${VAR} d'un serveur : erreur = serveur refusé.
pub(crate) type Expander<'a> = dyn Fn(&str) -> Result<String, String> + 'a;

fn parse_servers(json: &Value) -> Result<HashMap<String, ServerConfig>, String> {
    load_secrets_env();
    let mut result = HashMap::new();
    let servers_obj = json
        .get("mcpServers")
        .or_else(|| json.get("servers"))
        .unwrap_or(json);
    let servers = match servers_obj.as_object() {
        Some(m) => m,
        None => return Ok(result),
    };

    for (name, config) in servers {
        if name.starts_with('_') {
            continue;
        }
        if is_self(name, config) {
            eprintln!("[McpHub][INFO] Skipped self: {}", name);
            continue;
        }
        if config
            .get("disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            eprintln!("[McpHub][INFO] Skipped disabled: {}", name);
            continue;
        }
        if name.contains('@') {
            return Err(format!(
                "{} : le caractere @ est reserve aux instances des tenants (serveur@tenant)",
                name
            ));
        }
        let server =
            parse_server_entry(name, config, json.get("settings"), &|v| Ok(expand_vars(v)))?;
        result.insert(name.clone(), server);
    }
    Ok(result)
}

fn expand_string_map(
    value: Option<&Value>,
    expand: &Expander,
) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    if let Some(obj) = value.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), expand(s)?);
            }
        }
    }
    Ok(out)
}

fn request_timeout(config: &Value) -> Option<u64> {
    config
        .get("requestTimeoutSecs")
        .or_else(|| config.get("requestTimeout"))
        .and_then(|v| v.as_u64())
}

/// Une entrée de `mcpServers`, ${VAR} remplacés par `expand`. Utilisée pour la config
/// normale et pour les instances des tenants (avec leur propre coffre).
pub(crate) fn parse_server_entry(
    name: &str,
    config: &Value,
    settings: Option<&Value>,
    expand: &Expander,
) -> Result<ServerConfig, String> {
    if let Some(cmd) = config.get("command").and_then(|v| v.as_str()) {
        let args: Vec<String> = config
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(expand)
                    .collect::<Result<_, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let env = expand_string_map(config.get("env"), expand)?;
        let pool = config.get("pool").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let command = expand(cmd)?;
        let sandbox = crate::sandbox::resolve(
            name,
            config,
            settings.and_then(|s| s.get("sandbox")),
            &command,
            &args,
        )?;
        Ok(ServerConfig {
            command,
            args,
            env,
            pool,
            request_timeout_secs: request_timeout(config),
            sandbox,
            inherit_env: config
                .get("inheritEnv")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            url: None,
            headers: HashMap::new(),
            oauth: false,
            tenant: None,
        })
    } else if let Some(url) = config.get("url").and_then(|v| v.as_str()) {
        // Serveur distant natif : {"url": "...", "headers": {...}, "auth": "oauth" | "none"}
        let headers = expand_string_map(config.get("headers"), expand)?;
        let has_auth_header = headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"));
        let oauth = match config.get("auth").and_then(|v| v.as_str()) {
            Some(a) => a.eq_ignore_ascii_case("oauth"),
            None => !has_auth_header,
        };
        Ok(ServerConfig {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            pool: config.get("pool").and_then(|v| v.as_u64()).unwrap_or(1) as usize,
            request_timeout_secs: request_timeout(config),
            sandbox: None,
            inherit_env: Vec::new(),
            url: Some(expand(url)?),
            headers,
            oauth,
            tenant: None,
        })
    } else {
        Err(format!("{} : ni \"command\" ni \"url\"", name))
    }
}

/// Réglages de garde des outils de la config dédiée (pour la commande `McpHub pins`).
pub fn load_guard_config() -> crate::toolguard::GuardConfig {
    match load_dedicated() {
        Dedicated::Valid(c) => c.guard,
        _ => crate::toolguard::GuardConfig::default(),
    }
}

/// État de `config.json` de l'instance. Une config invalide n'est jamais confondue avec une
/// config absente : ce serait remplacer les serveurs choisis par ceux des éditeurs.
pub enum Dedicated {
    Missing,
    Invalid(String),
    Valid(Box<ProxyConfig>),
}

/// `config.json` brut, pour le modifier. `None` si absent. Illisible ou invalide = erreur :
/// repartir d'une config vide puis l'enregistrer effacerait tous les serveurs.
pub fn read_raw(path: &Path) -> Result<Option<Value>, String> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} illisible : {}", path.display(), e)),
    };
    let json: Value = serde_json::from_str(&content).map_err(|e| {
        format!(
            "{} : JSON invalide ({}), corrige-le avant toute modification",
            path.display(),
            e
        )
    })?;
    if !json.is_object() {
        return Err(format!(
            "{} : la racine doit etre un objet JSON",
            path.display()
        ));
    }
    Ok(Some(json))
}

static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Écriture atomique (fichier temporaire unique puis rename) : le surveillant de config ne lit
/// jamais un fichier à moitié écrit, et deux écritures simultanées ne se mélangent pas.
pub fn write_raw_atomic(path: &Path, config: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{} : {}", dir.display(), e))?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), seq));
    if let Err(e) = write_private_synced(&tmp, text.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{} : {}", tmp.display(), e));
    }
    crate::vault::restrict_permissions(&tmp);
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{} : {}", path.display(), e)
    })?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(handle) = fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

/// Sauvegardes gardées dans `backups/` à côté de la config.
const BACKUPS_KEPT: usize = 20;

/// Une seule modification protégée à la fois dans ce processus.
static GUARDED_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Empreinte courte du contenu d'un fichier de config ("" pour un fichier absent).
pub fn revision_of(bytes: Option<&[u8]>) -> String {
    use sha2::{Digest, Sha256};
    match bytes {
        None => String::new(),
        Some(b) => Sha256::digest(b)
            .iter()
            .take(8)
            .map(|x| format!("{:02x}", x))
            .collect(),
    }
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} illisible : {}", path.display(), e)),
    }
}

/// Config brute et son empreinte, pour une lecture suivie d'une modification protégée.
pub fn read_raw_with_revision(path: &Path) -> Result<(Option<Value>, String), String> {
    let bytes = read_bytes(path)?;
    let revision = revision_of(bytes.as_deref());
    Ok((read_raw(path)?, revision))
}

/// Copie du fichier actuel dans `backups/config-<horodatage>.json` (600), puis seules les
/// `BACKUPS_KEPT` plus récentes sont gardées.
fn backup_config(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} : pas de dossier parent", path.display()))?
        .join("backups");
    fs::create_dir_all(&dir).map_err(|e| format!("{} : {}", dir.display(), e))?;
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let stamp: String = crate::audit::iso_utc(d.as_secs(), d.subsec_millis())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let target = dir.join(format!("config-{}-{:06}.json", stamp, seq % 1_000_000));
    write_private_synced(&target, bytes).map_err(|e| format!("{} : {}", target.display(), e))?;
    let mut existing: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("{} : {}", dir.display(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("config-") && n.ends_with(".json"))
        })
        .collect();
    existing.sort();
    let excess = existing.len().saturating_sub(BACKUPS_KEPT);
    for old in existing.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
    Ok(target)
}

/// Résultat d'une modification protégée.
pub struct GuardedWrite<T> {
    pub value: T,
    /// Empreinte du fichier après écriture (inchangée si rien n'a été écrit).
    pub revision: String,
    pub backup: Option<PathBuf>,
}

/// Modification protégée de la config : refus si `expected` (empreinte lue plus tôt par
/// l'appelant) ne correspond plus, sauvegarde avant écriture, et refus si le fichier a changé
/// pendant la modification. `mutate` renvoie `false` en second élément pour ne rien écrire.
pub fn update_raw_guarded<T>(
    path: &Path,
    expected: Option<&str>,
    mutate: impl FnOnce(&mut Value) -> Result<(T, bool), String>,
) -> Result<GuardedWrite<T>, String> {
    let _lock = GUARDED_WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let before = read_bytes(path)?;
    let revision = revision_of(before.as_deref());
    if let Some(exp) = expected {
        if exp != revision {
            return Err(format!(
                "{} a change depuis la lecture (revision {} attendue, {} actuelle) : relire avec servers_list puis reessayer",
                path.display(),
                exp,
                revision
            ));
        }
    }
    let mut config = match &before {
        None => serde_json::json!({ "mcpServers": {} }),
        Some(_) => read_raw(path)?.unwrap_or_else(|| serde_json::json!({ "mcpServers": {} })),
    };
    let (value, write) = mutate(&mut config)?;
    if !write {
        return Ok(GuardedWrite {
            value,
            revision,
            backup: None,
        });
    }
    let backup = match &before {
        Some(bytes) => Some(backup_config(path, bytes)?),
        None => None,
    };
    if revision_of(read_bytes(path)?.as_deref()) != revision {
        return Err(format!(
            "{} modifie par un autre programme pendant l'operation : rien n'a ete ecrit, reessayer",
            path.display()
        ));
    }
    write_raw_atomic(path, &config)?;
    let revision = revision_of(read_bytes(path)?.as_deref());
    Ok(GuardedWrite {
        value,
        revision,
        backup,
    })
}

/// Fichier neuf, créé directement en 600 (la config peut contenir des secrets en clair) et
/// écrit sur disque avant le renommage, pour qu'une coupure ne laisse pas un config.json vide.
fn write_private_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub fn dedicated_config_path() -> PathBuf {
    crate::instance::hub_dir().join("config.json")
}

pub fn load_dedicated() -> Dedicated {
    load_dedicated_from_path(&dedicated_config_path())
}

fn load_dedicated_from_path(path: &Path) -> Dedicated {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Dedicated::Missing,
        Err(e) => return Dedicated::Invalid(format!("{} illisible : {}", path.display(), e)),
    };
    let json: Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(e) => return Dedicated::Invalid(format!("{} : JSON invalide : {}", path.display(), e)),
    };
    match config_from_json(&json, Some(path)) {
        Ok(c) => Dedicated::Valid(Box::new(c)),
        Err(e) => Dedicated::Invalid(format!("{} : {}", path.display(), e)),
    }
}

fn config_from_json(json: &Value, source_path: Option<&Path>) -> Result<ProxyConfig, String> {
    if !json.is_object() {
        return Err("la racine doit etre un objet JSON".into());
    }
    let servers = parse_servers(json)?;
    if let Some(path) = source_path {
        eprintln!(
            "[McpHub][INFO] Loaded {} servers from {}",
            servers.len(),
            path.display()
        );
    }

    let mut config = ProxyConfig {
        servers,
        guard: crate::toolguard::parse_guard_config(json),
        code_mode: crate::codemode::parse(json.get("settings")),
        ..Default::default()
    };
    if let Some(settings) = json.get("settings") {
        if let Some(mode) = settings.get("mode").and_then(|v| v.as_str()) {
            config.mode = match mode {
                "passthrough" => Mode::Passthrough,
                _ => Mode::Discover,
            };
        }
        if let Some(media_profile) = settings.get("mediaProfile").and_then(|v| v.as_str()) {
            config.media_profile = MediaProfile::from_str(media_profile);
        }
        if let Some(preload) = settings.get("preload") {
            config.preload = match preload {
                Value::String(value) if value.eq_ignore_ascii_case("none") => Preload::None,
                Value::String(value) if value.eq_ignore_ascii_case("all") => Preload::All,
                Value::String(value) if value.eq_ignore_ascii_case("auto") => Preload::Auto,
                Value::Array(values) => {
                    let names: Vec<String> = values
                        .iter()
                        .filter_map(|value| value.as_str().map(String::from))
                        .collect();
                    if names.is_empty() {
                        Preload::None
                    } else {
                        Preload::Some(names)
                    }
                }
                _ => config.preload.clone(),
            };
        }
        if let Some(keep) = settings.get("keepPreloadedWarm").and_then(|v| v.as_bool()) {
            config.keep_preloaded_warm = keep;
        }
        if let Some(count) = settings.get("preloadAutoCount").and_then(|v| v.as_u64()) {
            config.preload_auto_count = count as usize;
        }
        if let Some(timeout) = settings.get("idleTimeout").and_then(|v| v.as_u64()) {
            config.idle_timeout_ms = timeout * 1000;
        }
        if let Some(delay) = settings
            .get("preloadDelayMs")
            .or_else(|| settings.get("preloadDelay"))
            .and_then(|v| v.as_u64())
        {
            config.preload_delay_ms = delay;
        }
        if let Some(timeout) = settings
            .get("requestTimeoutSecs")
            .or_else(|| settings.get("requestTimeout"))
            .and_then(|v| v.as_u64())
        {
            config.request_timeout_secs = timeout.max(1);
        }
        if let Some(defer) = settings.get("deferAfterSecs").and_then(|v| v.as_u64()) {
            config.defer_after_secs = defer;
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
        if let Some(auto_update) = settings.get("autoUpdate") {
            config.auto_update = auto_update
                .as_bool()
                .ok_or("settings.autoUpdate doit valoir true ou false")?;
        }
        if let Some(response_cache) = settings.get("responseCache") {
            config.response_cache =
                crate::response_cache::CacheSettings::from_json(response_cache)?;
        }
        if let Some(permissions) = settings.get("permissions") {
            config.permissions = crate::permissions::PermissionsConfig::from_json(permissions)?;
        }
        if let Some(audit) = settings.get("audit") {
            config.audit = crate::audit::AuditConfig::from_json(audit);
        }
    }
    Ok(config)
}

fn get_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".cursor").join("mcp.json"));
        if cfg!(target_os = "macos") {
            if let Some(support) = dirs::data_dir() {
                paths.push(support.join("Claude").join("claude_desktop_config.json"));
            }
            paths.push(
                home.join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json"),
            );
        }
        paths.push(
            home.join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        );
        paths.push(home.join(".vscode").join("mcp.json"));
    }
    paths
}

/// Config au démarrage ou pour une commande : une config invalide donne zéro serveur (et une
/// erreur visible), jamais l'import des serveurs des éditeurs.
pub fn auto_detect() -> ProxyConfig {
    try_auto_detect().unwrap_or_else(|e| {
        eprintln!(
            "[McpHub][ERROR] config invalide, aucun serveur charge : {}",
            e
        );
        apply_env_overrides(ProxyConfig::default())
    })
}

/// Comme `auto_detect`, mais rend l'erreur d'une config invalide : le rechargement à chaud
/// garde alors la dernière config valide.
pub fn try_auto_detect() -> Result<ProxyConfig, String> {
    match load_dedicated() {
        Dedicated::Valid(config) => {
            let mode_str = match config.mode {
                Mode::Discover => "discover",
                Mode::Passthrough => "passthrough",
            };
            eprintln!(
                "[McpHub][INFO] Using dedicated config: {} servers, mode={}",
                config.servers.len(),
                mode_str
            );
            Ok(apply_env_overrides(*config))
        }
        Dedicated::Invalid(e) => Err(e),
        Dedicated::Missing => Ok(editors_config()),
    }
}

fn editors_config() -> ProxyConfig {
    let mut config = ProxyConfig::default();
    if crate::instance::current().name.is_some() {
        eprintln!(
            "[McpHub][WARN] No config in {}: a named instance never imports the editors' MCP servers.",
            crate::instance::hub_dir().display()
        );
        return config;
    }
    for path in &get_config_paths() {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&content) else {
            eprintln!("[McpHub][WARN] {} : JSON invalide, ignore", path.display());
            continue;
        };
        match parse_servers(&json) {
            Ok(servers) if !servers.is_empty() => {
                eprintln!(
                    "[McpHub][INFO] Found {} servers in {}",
                    servers.len(),
                    path.display()
                );
                config.servers.extend(servers);
            }
            Ok(_) => {}
            Err(e) => eprintln!("[McpHub][WARN] {} ignore : {}", path.display(), e),
        }
    }

    if config.servers.is_empty() {
        eprintln!("[McpHub][WARN] No MCP servers found.");
    } else {
        eprintln!(
            "[McpHub][INFO] Total: {} servers detected",
            config.servers.len()
        );
    }
    apply_env_overrides(config)
}

fn apply_env_overrides(mut config: ProxyConfig) -> ProxyConfig {
    if let Ok(mode) = std::env::var("MCP_ON_DEMAND_MODE") {
        config.mode = match mode.as_str() {
            "passthrough" => Mode::Passthrough,
            _ => Mode::Discover,
        };
    }
    if let Ok(preload) = std::env::var("MCP_ON_DEMAND_PRELOAD") {
        config.preload = match preload.as_str() {
            "none" => Preload::None,
            "auto" => Preload::Auto,
            _ => Preload::All,
        };
    }
    if let Ok(media_profile) = std::env::var("MCPHUB_MEDIA_PROFILE")
        .or_else(|_| std::env::var("MCP_ON_DEMAND_MEDIA_PROFILE"))
    {
        config.media_profile = MediaProfile::from_str(&media_profile);
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
        assert!(is_self(
            "test",
            &json!({"command": "node", "args": ["mcphub"]})
        ));
        assert!(is_self(
            "client",
            &json!({"command": "node", "args": ["/Users/me/McpHub/client/index.js"]})
        ));
        assert!(
            !is_self(
                "simtrail",
                &json!({"command": "/Users/me/.local/bin/simtrail", "args": ["mcp", "--roots-meta", "com.soflution.mcphub/roots"]})
            ),
            "un argument qui cite McpHub ne fait pas du serveur une copie de McpHub"
        );
        let bundle = crate::registry::servers_dir().join("depsonar/current/dist/index.js");
        assert!(!is_self(
            "depsonar",
            &json!({"command": "node", "args": [bundle.to_string_lossy()]})
        ));
    }

    #[test]
    fn test_parse_servers() {
        let json = json!({
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "requestTimeoutSecs": 240,
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

        let servers = parse_servers(&json).unwrap();

        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("github"));
        assert!(!servers.contains_key("disabled_server")); // skipped disabled
        assert!(!servers.contains_key("McpHub")); // skipped self

        let github = &servers["github"];
        assert_eq!(github.command, "npx");
        assert_eq!(github.args.len(), 2);
        assert_eq!(github.env.get("GITHUB_TOKEN").unwrap(), "123");
        assert_eq!(github.request_timeout_secs, Some(240));
    }

    #[test]
    fn preload_formats_acceptes() {
        let parse = |settings: Value| {
            config_from_json(&json!({ "mcpServers": {}, "settings": settings }), None)
                .expect("config valide")
        };
        assert_eq!(parse(json!({})).preload, Preload::None);
        assert_eq!(parse(json!({ "preload": "all" })).preload, Preload::All);
        assert_eq!(parse(json!({ "preload": "NONE" })).preload, Preload::None);
        assert_eq!(parse(json!({ "preload": [] })).preload, Preload::None);
        assert_eq!(
            parse(json!({ "preload": ["a", "b"] })).preload,
            Preload::Some(vec!["a".into(), "b".into()])
        );
        let auto = parse(json!({ "preload": "auto" }));
        assert_eq!(auto.preload, Preload::Auto);
        assert_eq!(auto.preload_auto_count, 5);
        assert_eq!(
            parse(json!({ "preload": "auto", "preloadAutoCount": 3 })).preload_auto_count,
            3
        );
    }

    #[test]
    fn test_parse_servers_no_servers() {
        let json = json!({"servers": {}});
        let servers = parse_servers(&json).unwrap();
        assert!(servers.is_empty());
        // Sans clé servers/mcpServers, la racine est lue comme la liste des serveurs : une
        // entrée qui n'en est pas une est signalée au lieu d'être ignorée.
        assert!(parse_servers(&json!({"otherKey": "value"})).is_err());
    }

    #[test]
    fn config_absente_invalide_ou_vide_bien_distinguees() {
        let dir = std::env::temp_dir().join(format!("mcphub-config-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let _ = fs::remove_file(&path);
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Missing
        ));

        fs::write(&path, "{\"servers\": {").unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(
            &path,
            r#"{"servers": {"a": {"command": "x", "sandbox": "sheild"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(&path, r#"{"servers": {"a": {"args": ["x"]}}}"#).unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(
            &path,
            r#"{"servers": {}, "settings": {"permissions": {"rules": [{"action": "dney"}]}}}"#,
        )
        .unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(
            &path,
            r#"{"servers": {}, "settings": {"autoUpdate": "no"}}"#,
        )
        .unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(
            &path,
            r#"{"servers": {}, "settings": {"responseCache": {"ttlSecs": 99999}}}"#,
        )
        .unwrap();
        assert!(matches!(
            load_dedicated_from_path(&path),
            Dedicated::Invalid(_)
        ));

        fs::write(&path, r#"{"servers": {}, "settings": {"responseCache": {"ttlSecs": 0, "tools": ["context7__resolve-library-id"]}}}"#).unwrap();
        match load_dedicated_from_path(&path) {
            Dedicated::Valid(c) => {
                assert_eq!(c.response_cache.ttl_secs, 0);
                assert!(c
                    .response_cache
                    .tools
                    .contains("context7__resolve-library-id"));
            }
            _ => panic!("responseCache valide"),
        }

        fs::write(
            &path,
            r#"{"servers": {}, "settings": {"autoUpdate": false}}"#,
        )
        .unwrap();
        match load_dedicated_from_path(&path) {
            Dedicated::Valid(c) => assert!(!c.auto_update),
            _ => panic!("autoUpdate: false est valide"),
        }

        fs::write(&path, r#"{"servers": {}}"#).unwrap();
        match load_dedicated_from_path(&path) {
            Dedicated::Valid(c) => assert!(c.servers.is_empty() && c.auto_update),
            _ => panic!("une config sans serveur est valide"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_media_profile_config_parsing() {
        let parsed = config_from_json(
            &json!({
                "servers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-github"]
                    }
                },
                "settings": {
                    "mediaProfile": "cursor-rich"
                }
            }),
            None,
        )
        .expect("parse config");
        assert_eq!(parsed.media_profile, MediaProfile::CursorRich);
    }

    #[test]
    fn test_global_request_timeout_config_parsing() {
        let parsed = config_from_json(
            &json!({
                "servers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-github"]
                    }
                },
                "settings": {
                    "requestTimeoutSecs": 180
                }
            }),
            None,
        )
        .expect("parse config");

        assert_eq!(parsed.request_timeout_secs, 180);
    }

    #[test]
    fn ecriture_protegee_sauvegarde_et_detecte_les_conflits() {
        let dir = std::env::temp_dir().join(format!("mcphub-guarded-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"mcpServers":{"a":{"command":"x"}}}"#).unwrap();
        let (_, rev) = read_raw_with_revision(&path).unwrap();
        assert_eq!(rev.len(), 16);

        let done = update_raw_guarded(&path, Some(&rev), |c| {
            c["mcpServers"]["b"] = json!({"command": "y"});
            Ok(((), true))
        })
        .unwrap();
        assert_ne!(done.revision, rev);
        let backup = done.backup.expect("sauvegarde");
        assert!(fs::read_to_string(&backup).unwrap().contains("\"a\""));
        assert!(!fs::read_to_string(&backup).unwrap().contains("\"b\""));

        // Ancienne révision : refus, rien d'écrit.
        let err = update_raw_guarded(&path, Some(&rev), |c| {
            c["mcpServers"]["c"] = json!({});
            Ok(((), true))
        })
        .err()
        .expect("conflit");
        assert!(err.contains("a change"), "{}", err);
        assert!(!fs::read_to_string(&path).unwrap().contains("\"c\""));

        // Rien à écrire : pas de sauvegarde, révision inchangée.
        let same = update_raw_guarded(&path, None, |_| Ok(((), false))).unwrap();
        assert!(same.backup.is_none());
        assert_eq!(same.revision, done.revision);

        for _ in 0..(BACKUPS_KEPT + 3) {
            update_raw_guarded(&path, None, |c| {
                c["n"] = json!(c["n"].as_u64().unwrap_or(0) + 1);
                Ok(((), true))
            })
            .unwrap();
        }
        let kept = fs::read_dir(dir.join("backups")).unwrap().count();
        assert_eq!(kept, BACKUPS_KEPT);
        let _ = fs::remove_dir_all(&dir);
    }
}
