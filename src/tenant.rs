//! Tenants : les comptes d'un client (Cloudflare, Supabase, Stripe…) isolés de ceux de McpHub.
//!
//! Un projet de `projects.json` marqué `"tenant": "<id>"` déclare, serveur par serveur, les
//! secrets de son propre coffre (`credentials`). Depuis un dossier de ce projet, McpHub lance
//! une instance séparée `serveur@<id>` avec ces secrets, et refuse tout autre serveur que ceux
//! du tenant et les `sharedServers`. Hors de ces dossiers (ou sans dossier connu), aucune
//! instance de tenant n'est atteignable.
//!
//! Le démon est l'autorité : il lit `projects.json` lui-même et déduit le contexte du dossier
//! transmis par le client (`_mcphub_context`). Le paramètre `project` n'entre jamais en compte.
//!
//! Une instance de tenant ne reçoit que : l'environnement système de base, les secrets du
//! tenant, `PATH` et les variables listées dans `keepEnv`. Tout `${VAR}` non couvert par le
//! coffre du tenant bloque son démarrage. `HOME` et `XDG_*` pointent vers un dossier propre au
//! tenant, pour que les logins enregistrés sur disque (gh, wrangler, supabase…) ne servent pas.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::config::ServerConfig;
use crate::vault::Scope;

/// Préfixe des références de secrets dans `credentials` : `vault:<tenant>/<NOM>`.
const VAULT_REF_PREFIX: &str = "vault:";
/// Variables non secrètes toujours reprises de la config du serveur.
const ALWAYS_KEPT_ENV: &[&str] = &["PATH"];
/// Durée après laquelle la config d'une instance est relue (secret modifié par la CLI).
pub const INSTANCE_CONFIG_TTL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Noms
// ---------------------------------------------------------------------------

/// Même règle que les instances nommées : 1 à 32 minuscules, chiffres ou tirets.
pub fn is_valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn is_instance_name(name: &str) -> bool {
    name.contains('@')
}

pub fn instance_name(server: &str, tenant: &str) -> String {
    format!("{}@{}", server, tenant)
}

/// `serveur@tenant` → (serveur, tenant).
pub fn split_instance(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('@')
}

/// Dossier McpHub du tenant (index du coffre, secrets.env hors macOS).
pub fn tenant_dir(id: &str) -> PathBuf {
    crate::instance::hub_dir().join("tenants").join(id)
}

/// HOME isolé des instances du tenant. Hors de `~/.McpHub`, interdit aux serveurs isolés.
pub fn tenant_home(id: &str) -> PathBuf {
    if let Ok(root) = std::env::var("MCPHUB_TENANT_HOMES") {
        if !root.is_empty() {
            return PathBuf::from(root).join(id).join("home");
        }
    }
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    let folder = match &crate::instance::current().name {
        Some(name) => format!("McpHub-{}", name),
        None => "McpHub".to_string(),
    };
    base.join(folder).join("tenants").join(id).join("home")
}

pub fn projects_path() -> PathBuf {
    match std::env::var("MCPHUB_PROJECTS") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::instance::hub_dir().join("projects.json"),
    }
}

// ---------------------------------------------------------------------------
// Déclaration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TenantDef {
    pub id: String,
    /// Nom du projet dans projects.json.
    pub project: String,
    pub workspaces: Vec<PathBuf>,
    /// serveur → (variable → nom du secret dans le coffre du tenant).
    pub credentials: BTreeMap<String, BTreeMap<String, String>>,
    /// serveur → variables non secrètes reprises de la config du serveur.
    pub keep_env: BTreeMap<String, Vec<String>>,
    /// Serveurs sans identifiants, utilisables tels quels depuis le dossier du tenant.
    pub shared_servers: BTreeSet<String>,
}

impl TenantDef {
    pub fn owns(&self, server: &str) -> bool {
        self.credentials.contains_key(server)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tenants {
    pub tenants: Vec<TenantDef>,
}

impl Tenants {
    pub fn get(&self, id: &str) -> Option<&TenantDef> {
        self.tenants.iter().find(|t| t.id == id)
    }

    /// Tenant dont un dossier contient `dir` (`/Clients/Acme2` n'est pas dans `/Clients/Acme`).
    pub fn for_dir(&self, dir: &Path) -> Option<&TenantDef> {
        let dir = dir_key(dir);
        self.tenants
            .iter()
            .flat_map(|t| t.workspaces.iter().map(move |w| (t, dir_key(w))))
            .filter(|(_, w)| dir.starts_with(w))
            // Le dossier le plus précis l'emporte.
            .max_by_key(|(_, w)| w.components().count())
            .map(|(t, _)| t)
    }
}

/// Chemin absolu sans `.` ni `..`, sans accès disque (le dossier peut ne pas exister).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Forme de comparaison d'un dossier : liens symboliques résolus (sur le plus long ancêtre
/// existant, le dossier pouvant ne pas exister encore) et, sur macOS (APFS insensible à la
/// casse), en minuscules. Un lien ou une autre casse ne fait pas sortir du dossier d'un tenant.
fn dir_key(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut existing = path.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let resolved = loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            break rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (
            existing.file_name().map(|f| f.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break path.clone(),
        }
    };
    if cfg!(target_os = "macos") {
        PathBuf::from(resolved.to_string_lossy().to_lowercase())
    } else {
        resolved
    }
}

/// Nom de secret d'une référence `vault:<tenant>/<NOM>` : le tenant doit être le sien.
fn parse_vault_ref(
    tenant: &str,
    server: &str,
    var: &str,
    reference: &str,
) -> Result<String, String> {
    let rest = reference.strip_prefix(VAULT_REF_PREFIX).ok_or_else(|| {
        format!(
            "tenant {} : {}.{} doit valoir \"vault:{}/NOM\" (jamais une valeur en clair)",
            tenant, server, var, tenant
        )
    })?;
    let (owner, name) = rest.split_once('/').ok_or_else(|| {
        format!(
            "tenant {} : {}.{} : reference \"{}\" sans /NOM",
            tenant, server, var, reference
        )
    })?;
    if owner != tenant {
        return Err(format!(
            "tenant {} : {}.{} pointe vers le coffre de \"{}\" : un tenant n'utilise que son propre coffre",
            tenant, server, var, owner
        ));
    }
    if !crate::vault::is_valid_name(name) {
        return Err(format!(
            "tenant {} : nom de secret invalide \"{}\"",
            tenant, name
        ));
    }
    Ok(name.to_string())
}

fn string_list(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| format!("{} : liste de textes attendue", what))
            })
            .collect(),
        Some(_) => Err(format!("{} : liste de textes attendue", what)),
    }
}

/// Lit les tenants d'un `projects.json`. Toute incohérence est une erreur : ignorer une
/// déclaration lèverait en silence le cloisonnement qu'elle demande.
pub fn parse_projects(json: &Value) -> Result<Tenants, String> {
    let Some(projects) = json.get("projects") else {
        return Ok(Tenants::default());
    };
    let projects = projects
        .as_object()
        .ok_or("\"projects\" doit etre un objet")?;
    let mut tenants: Vec<TenantDef> = Vec::new();
    for (project, cfg) in projects {
        let Some(tenant) = cfg.get("tenant") else {
            for key in ["credentials", "keepEnv", "sharedServers"] {
                if cfg.get(key).is_some() {
                    return Err(format!("projet {} : \"{}\" sans \"tenant\"", project, key));
                }
            }
            continue;
        };
        let id = tenant
            .as_str()
            .filter(|id| is_valid_id(id))
            .ok_or_else(|| {
                format!(
                    "projet {} : \"tenant\" doit etre un id (1 a 32 minuscules, chiffres, tirets)",
                    project
                )
            })?
            .to_string();
        if tenants.iter().any(|t| t.id == id) {
            return Err(format!("tenant {} declare par deux projets", id));
        }
        let workspaces: Vec<PathBuf> = string_list(
            cfg.get("workspaces"),
            &format!("projet {} : workspaces", project),
        )?
        .into_iter()
        .map(PathBuf::from)
        .collect();
        if workspaces.is_empty() {
            return Err(format!("tenant {} : aucun dossier (workspaces)", id));
        }
        for w in &workspaces {
            if !w.is_absolute() {
                return Err(format!(
                    "tenant {} : dossier non absolu {}",
                    id,
                    w.display()
                ));
            }
        }
        let mut credentials = BTreeMap::new();
        if let Some(creds) = cfg.get("credentials") {
            let creds = creds
                .as_object()
                .ok_or_else(|| format!("tenant {} : \"credentials\" doit etre un objet", id))?;
            for (server, vars) in creds {
                if is_instance_name(server) {
                    return Err(format!(
                        "tenant {} : nom de serveur invalide {}",
                        id, server
                    ));
                }
                let vars = vars.as_object().ok_or_else(|| {
                    format!("tenant {} : credentials.{} doit etre un objet", id, server)
                })?;
                let mut map = BTreeMap::new();
                for (var, reference) in vars {
                    if !crate::vault::is_valid_name(var) {
                        return Err(format!(
                            "tenant {} : variable invalide {}.{}",
                            id, server, var
                        ));
                    }
                    let reference = reference.as_str().ok_or_else(|| {
                        format!("tenant {} : {}.{} doit etre un texte", id, server, var)
                    })?;
                    map.insert(var.clone(), parse_vault_ref(&id, server, var, reference)?);
                }
                credentials.insert(server.clone(), map);
            }
        }
        let mut keep_env = BTreeMap::new();
        if let Some(keep) = cfg.get("keepEnv") {
            let keep = keep
                .as_object()
                .ok_or_else(|| format!("tenant {} : \"keepEnv\" doit etre un objet", id))?;
            for (server, vars) in keep {
                if !credentials.contains_key(server) {
                    return Err(format!(
                        "tenant {} : keepEnv.{} sans credentials.{}",
                        id, server, server
                    ));
                }
                keep_env.insert(
                    server.clone(),
                    string_list(Some(vars), &format!("tenant {} : keepEnv.{}", id, server))?,
                );
            }
        }
        let shared_servers: BTreeSet<String> = string_list(
            cfg.get("sharedServers"),
            &format!("tenant {} : sharedServers", id),
        )?
        .into_iter()
        .collect();
        if let Some(both) = shared_servers.iter().find(|s| credentials.contains_key(*s)) {
            return Err(format!(
                "tenant {} : {} est a la fois dans credentials et sharedServers",
                id, both
            ));
        }
        tenants.push(TenantDef {
            id,
            project: project.clone(),
            workspaces,
            credentials,
            keep_env,
            shared_servers,
        });
    }
    // Un même dossier ne peut pas appartenir à deux tenants.
    for (i, a) in tenants.iter().enumerate() {
        for b in &tenants[i + 1..] {
            for wa in &a.workspaces {
                for wb in &b.workspaces {
                    let (ka, kb) = (dir_key(wa), dir_key(wb));
                    if ka.starts_with(&kb) || kb.starts_with(&ka) {
                        return Err(format!(
                            "les dossiers des tenants {} et {} se chevauchent ({} / {})",
                            a.id,
                            b.id,
                            wa.display(),
                            wb.display()
                        ));
                    }
                }
            }
        }
    }
    Ok(Tenants { tenants })
}

pub fn load_from(path: &Path) -> Result<Tenants, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let json: Value = serde_json::from_str(&text)
                .map_err(|e| format!("{} : JSON invalide ({})", path.display(), e))?;
            parse_projects(&json).map_err(|e| format!("{} : {}", path.display(), e))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tenants::default()),
        Err(e) => Err(format!("{} illisible : {}", path.display(), e)),
    }
}

// ---------------------------------------------------------------------------
// Contexte d'un appel et routage
// ---------------------------------------------------------------------------

/// Dossiers transmis par le client avec chaque appel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallOrigin {
    /// `working_dir` donné par l'agent.
    pub working_dir: Option<PathBuf>,
    /// Dossier courant du client McpHub (celui où l'agent a été lancé).
    pub cwd: Option<PathBuf>,
}

impl CallOrigin {
    pub fn from_args(args: &Value) -> Self {
        let ctx = args.get("_mcphub_context");
        let dir = |key: &str| {
            ctx.and_then(|c| c.get(key))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        Self {
            working_dir: dir("working_dir"),
            cwd: dir("cwd"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.working_dir.is_none() && self.cwd.is_none()
    }

    pub fn to_json(&self) -> Value {
        let mut ctx = serde_json::Map::new();
        if let Some(w) = &self.working_dir {
            ctx.insert("working_dir".into(), json!(w.to_string_lossy()));
        }
        if let Some(c) = &self.cwd {
            ctx.insert("cwd".into(), json!(c.to_string_lossy()));
        }
        Value::Object(ctx)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Context {
    /// Comptes McpHub (Soflution) : aucune instance de tenant atteignable.
    Hub,
    Tenant(Arc<TenantDef>),
}

impl Context {
    pub fn tenant_id(&self) -> Option<&str> {
        match self {
            Context::Hub => None,
            Context::Tenant(t) => Some(&t.id),
        }
    }
}

/// Contexte d'un appel. `working_dir` prime ; sans lui, le dossier courant du client.
/// Un client lancé dans le dossier d'un tenant ne peut pas viser un autre dossier.
pub fn resolve_context(tenants: &Tenants, origin: &CallOrigin) -> Result<Context, String> {
    let from_cwd = origin.cwd.as_deref().and_then(|d| tenants.for_dir(d));
    let from_wd = origin
        .working_dir
        .as_deref()
        .and_then(|d| tenants.for_dir(d));
    let chosen = match (&origin.working_dir, from_wd, from_cwd) {
        (Some(_), wd, Some(cwd)) if wd.map(|t| &t.id) != Some(&cwd.id) => {
            return Err(format!(
                "appel refuse : l'agent tourne dans le dossier du tenant {} mais working_dir vise un autre perimetre",
                cwd.id
            ));
        }
        (Some(_), wd, _) => wd,
        (None, _, cwd) => cwd,
    };
    Ok(match chosen {
        Some(t) => Context::Tenant(Arc::new(t.clone())),
        None => Context::Hub,
    })
}

/// Où part un appel.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Serveur McpHub normal.
    Hub(String),
    /// Instance `serveur@tenant`.
    Tenant {
        server: String,
        tenant: String,
        instance: String,
    },
}

impl Route {
    /// Clé du processus, du cache et des métriques.
    pub fn key(&self) -> &str {
        match self {
            Route::Hub(s) => s,
            Route::Tenant { instance, .. } => instance,
        }
    }

    pub fn server(&self) -> &str {
        match self {
            Route::Hub(s) => s,
            Route::Tenant { server, .. } => server,
        }
    }
}

/// Routage d'un serveur (nom canonique de la config) selon le contexte.
pub fn route(ctx: &Context, server: &str) -> Result<Route, String> {
    if is_instance_name(server) {
        return Err(format!(
            "serveur \"{}\" refuse : une instance de tenant ne s'appelle jamais par son nom, elle se deduit du dossier de travail",
            server
        ));
    }
    match ctx {
        Context::Hub => Ok(Route::Hub(server.to_string())),
        Context::Tenant(t) if t.owns(server) => Ok(Route::Tenant {
            server: server.to_string(),
            tenant: t.id.clone(),
            instance: instance_name(server, &t.id),
        }),
        Context::Tenant(t) if t.shared_servers.contains(server) => Ok(Route::Hub(server.to_string())),
        Context::Tenant(t) => Err(format!(
            "serveur \"{}\" refuse dans le dossier du tenant {} : seuls ses serveurs ({}) et les serveurs partages ({}) y repondent",
            server,
            t.id,
            join_or_none(t.credentials.keys()),
            join_or_none(t.shared_servers.iter()),
        )),
    }
}

/// Serveur visible dans la recherche depuis ce contexte.
pub fn visible(ctx: &Context, server: &str) -> bool {
    match ctx {
        Context::Hub => !is_instance_name(server),
        Context::Tenant(t) => t.owns(server) || t.shared_servers.contains(server),
    }
}

fn join_or_none<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = items.map(|s| s.as_str()).collect();
    if v.is_empty() {
        "aucun".into()
    } else {
        v.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Config d'une instance
// ---------------------------------------------------------------------------

/// Lecture d'un secret du coffre du tenant.
pub type SecretLookup<'a> = dyn Fn(&str) -> Result<Option<String>, String> + 'a;

/// Config de `serveur@tenant` dérivée de la config brute du serveur : environnement en liste
/// blanche, ${VAR} résolus uniquement depuis le coffre du tenant, HOME isolé.
pub fn derive_config(
    tenant: &TenantDef,
    server: &str,
    raw_server: &Value,
    settings: Option<&Value>,
    lookup: &SecretLookup,
) -> Result<ServerConfig, String> {
    let vars = tenant
        .credentials
        .get(server)
        .ok_or_else(|| format!("{} n'est pas un serveur du tenant {}", server, tenant.id))?;
    let mut secrets: HashMap<String, String> = HashMap::new();
    for (var, secret) in vars {
        let value = lookup(secret)?.ok_or_else(|| {
            format!(
                "secret {} absent du coffre du tenant {} (McpHub vault set --tenant {} {})",
                secret, tenant.id, tenant.id, secret
            )
        })?;
        secrets.insert(var.clone(), value);
    }
    let keep: BTreeSet<&str> = tenant
        .keep_env
        .get(server)
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();

    // Liste blanche AVANT le remplacement des ${VAR} : une variable écartée ne doit ni passer
    // ni bloquer le démarrage. Une variable fournie par le coffre remplace la valeur de la
    // config telle quelle (ajoutée plus bas, jamais réinterprétée).
    let mut raw = raw_server.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.remove("inheritEnv");
        if let Some(env) = obj.get_mut("env").and_then(|e| e.as_object_mut()) {
            env.retain(|k, _| {
                !secrets.contains_key(k)
                    && (keep.contains(k.as_str()) || ALWAYS_KEPT_ENV.contains(&k.as_str()))
            });
        }
    }
    let expand = |input: &str| -> Result<String, String> {
        let mut missing: Vec<String> = Vec::new();
        let out = crate::config::expand_with(input, &mut |var| match secrets.get(var) {
            Some(v) => Some(v.clone()),
            None => {
                missing.push(var.to_string());
                None
            }
        });
        if missing.is_empty() {
            Ok(out)
        } else {
            Err(format!(
                "{}@{} : ${{{}}} n'est pas fourni par le coffre du tenant (a declarer dans credentials.{})",
                server,
                tenant.id,
                missing.join("}, ${"),
                server
            ))
        }
    };
    let mut config = crate::config::parse_server_entry(server, &raw, settings, &expand)?;
    config.inherit_env.clear();
    config.tenant = Some(tenant.id.clone());
    if config.url.is_none() {
        for (var, value) in &secrets {
            config.env.insert(var.clone(), value.clone());
        }
        let home = tenant_home(&tenant.id);
        config
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        config.env.insert(
            "XDG_CONFIG_HOME".into(),
            home.join(".config").to_string_lossy().into_owned(),
        );
        config.env.insert(
            "XDG_DATA_HOME".into(),
            home.join(".local/share").to_string_lossy().into_owned(),
        );
        config.env.insert(
            "XDG_STATE_HOME".into(),
            home.join(".local/state").to_string_lossy().into_owned(),
        );
        config.env.insert(
            "XDG_CACHE_HOME".into(),
            home.join(".cache").to_string_lossy().into_owned(),
        );
        // Le cache npm (paquets publics, aucun jeton) reste partagé : npx garde sa vitesse.
        if let Some(real_home) = dirs::home_dir() {
            config
                .env
                .entry("npm_config_cache".into())
                .or_insert_with(|| real_home.join(".npm").to_string_lossy().into_owned());
        }
        config.env.insert("MCPHUB_TENANT".into(), tenant.id.clone());
    }
    Ok(config)
}

/// Config brute d'un serveur et réglages globaux, lus dans config.json.
pub fn raw_server_config(server: &str) -> Result<(Value, Option<Value>), String> {
    let path = crate::config::dedicated_config_path();
    let json = crate::config::read_raw(&path)?.ok_or_else(|| {
        format!(
            "{} absent : les tenants exigent une config McpHub dediee",
            path.display()
        )
    })?;
    let servers = json
        .get("mcpServers")
        .or_else(|| json.get("servers"))
        .unwrap_or(&json);
    let raw = servers
        .get(server)
        .cloned()
        .ok_or_else(|| format!("serveur {} absent de {}", server, path.display()))?;
    if raw
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(format!(
            "serveur {} desactive dans {}",
            server,
            path.display()
        ));
    }
    Ok((raw, json.get("settings").cloned()))
}

/// Config d'une instance avec le vrai coffre du tenant.
pub fn derive_from_vault(tenant: &TenantDef, server: &str) -> Result<ServerConfig, String> {
    let (raw, settings) = raw_server_config(server)?;
    let scope = Scope::Tenant(tenant.id.clone());
    let config = derive_config(tenant, server, &raw, settings.as_ref(), &|name| {
        crate::vault::get_in(&scope, name)
    })?;
    if config.url.is_none() {
        let home = tenant_home(&tenant.id);
        std::fs::create_dir_all(&home).map_err(|e| format!("{} : {}", home.display(), e))?;
        crate::vault::restrict_permissions(&home);
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Registre du démon
// ---------------------------------------------------------------------------

type Stamp = Option<(SystemTime, u64)>;
type CachedTenants = Option<(Stamp, Arc<Result<Tenants, String>>)>;

/// `projects.json` relu à chaque modification, et date de dérivation de chaque instance.
pub struct Registry {
    path: PathBuf,
    cached: Mutex<CachedTenants>,
    derived_at: Mutex<HashMap<String, Instant>>,
}

impl Registry {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
            derived_at: Mutex::new(HashMap::new()),
        }
    }

    pub fn tenants(&self) -> Arc<Result<Tenants, String>> {
        let stamp: Stamp = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((s, t)) = cached.as_ref() {
            if *s == stamp {
                return t.clone();
            }
        }
        let loaded = Arc::new(load_from(&self.path));
        if let Err(e) = loaded.as_ref() {
            eprintln!("[McpHub][ERROR] tenants : {}", e);
        }
        *cached = Some((stamp, loaded.clone()));
        loaded
    }

    /// Contexte d'un appel. Sans dossier transmis : McpHub. `projects.json` invalide avec un
    /// dossier transmis : refus (on ne sait pas si l'appel vient d'un tenant).
    pub fn context(&self, origin: &CallOrigin) -> Result<Context, String> {
        if origin.is_empty() {
            return Ok(Context::Hub);
        }
        match self.tenants().as_ref() {
            Ok(tenants) => resolve_context(tenants, origin),
            Err(e) => Err(format!("appel refuse, projects.json invalide : {}", e)),
        }
    }

    /// Vrai si la config de l'instance doit être (re)calculée.
    pub fn needs_derivation(&self, instance: &str, present: bool) -> bool {
        if !present {
            return true;
        }
        let derived = self.derived_at.lock().unwrap_or_else(|e| e.into_inner());
        derived
            .get(instance)
            .is_none_or(|at| at.elapsed() >= INSTANCE_CONFIG_TTL)
    }

    /// Oublie les dérivations du tenant : ses instances relisent coffre et config au prochain appel.
    pub fn forget_tenant(&self, id: &str) {
        self.derived_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|instance, _| split_instance(instance).is_none_or(|(_, t)| t != id));
    }

    pub fn mark_derived(&self, instance: &str) {
        self.derived_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(instance.to_string(), Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Édition depuis le tableau de bord (écran Client accounts)
// ---------------------------------------------------------------------------

/// Refus d'écriture : projects.json a changé depuis l'affichage de l'écran.
pub const STALE_PROJECTS: &str =
    "projects.json a change depuis l'affichage : recharger l'ecran avant de modifier";

/// Écritures du tableau de bord sérialisées : deux requêtes ne lisent pas la même version.
static EDIT_LOCK: Mutex<()> = Mutex::new(());

/// Empreinte de projects.json (date de modification et taille). Une écriture du tableau de bord
/// est refusée si le fichier a changé depuis l'affichage (édition à la main, CLI).
pub fn projects_version(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let nanos = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}-{}", nanos, m.len())
        }
        Err(_) => "absent".into(),
    }
}

/// Lit projects.json, applique `edit` et l'écrit (validation, sauvegarde, écriture atomique),
/// seulement si le fichier n'a pas changé depuis `expected_version`.
pub fn edit_projects(
    path: &Path,
    expected_version: &str,
    label: &str,
    edit: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<Option<PathBuf>, String> {
    let _guard = EDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if projects_version(path) != expected_version {
        return Err(STALE_PROJECTS.into());
    }
    let mut json = read_projects_raw(path)?;
    edit(&mut json)?;
    write_projects(path, &json, label)
}

/// Ajout d'un tenant depuis le tableau de bord : mêmes contrôles que `McpHub tenant add`.
pub fn add_from_dashboard(
    path: &Path,
    expected_version: &str,
    id: &str,
    dir: &Path,
    project: &str,
) -> Result<Option<PathBuf>, String> {
    if !dir.is_absolute() {
        return Err(format!("dossier non absolu : {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("dossier introuvable : {}", dir.display()));
    }
    if project.trim().is_empty() {
        return Err("nom de projet vide".into());
    }
    let dir = normalize(dir);
    edit_projects(path, expected_version, &format!("tenant-{}", id), |json| {
        add_tenant(json, id, &dir, project)
    })
}

/// Serveurs déclarés dans config.json (hors serveurs désactivés), config brute.
pub fn configured_servers() -> Result<BTreeMap<String, Value>, String> {
    let path = crate::config::dedicated_config_path();
    let Some(json) = crate::config::read_raw(&path)? else {
        return Ok(BTreeMap::new());
    };
    let servers = json
        .get("mcpServers")
        .or_else(|| json.get("servers"))
        .unwrap_or(&json);
    let Some(servers) = servers.as_object() else {
        return Err(format!("{} : liste de serveurs illisible", path.display()));
    };
    Ok(servers
        .iter()
        .filter(|(name, raw)| {
            !is_instance_name(name)
                && (raw.get("command").is_some() || raw.get("url").is_some())
                && !raw
                    .get("disabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
        })
        .map(|(name, raw)| (name.clone(), raw.clone()))
        .collect())
}

fn collect_refs(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            crate::config::expand_with(s, &mut |var| {
                if crate::vault::is_valid_name(var) {
                    out.insert(var.to_string());
                }
                None
            });
        }
        Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// Variables qu'un serveur attend d'un tenant (noms seulement). `required` : les `${VAR}` de
/// la config (args, url, en-têtes) et les clés d'env dont la valeur est un `${…}` ; `optional` :
/// les autres clés d'env, écartées par la liste blanche sauf si le tenant les fournit (un
/// chemin de clé en dur, par exemple).
pub fn server_variables(raw: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut required = BTreeSet::new();
    let mut optional = BTreeSet::new();
    let Some(obj) = raw.as_object() else {
        return (required, optional);
    };
    for (key, value) in obj {
        if key != "env" {
            collect_refs(value, &mut required);
        }
    }
    if let Some(env) = obj.get("env").and_then(|e| e.as_object()) {
        for (key, value) in env {
            if ALWAYS_KEPT_ENV.contains(&key.as_str()) || !crate::vault::is_valid_name(key) {
                continue;
            }
            if value.as_str().is_some_and(|s| s.contains("${")) {
                required.insert(key.clone());
            } else {
                optional.insert(key.clone());
            }
        }
    }
    optional.retain(|k| !required.contains(k));
    (required, optional)
}

/// Remplace les serveurs d'un tenant : `servers` (serveur → variables fournies par son coffre)
/// et `shared` (serveurs utilisés tels quels). Une variable déjà déclarée garde son secret ;
/// une nouvelle pointe vers le secret du même nom. Rien ne change en cas d'erreur.
pub fn set_servers(
    json: &mut Value,
    id: &str,
    servers: &BTreeMap<String, Vec<String>>,
    shared: &[String],
    known: &BTreeSet<String>,
) -> Result<(), String> {
    let mut candidate = json.clone();
    let tenants = parse_projects(&candidate)?;
    let def = tenants
        .get(id)
        .ok_or_else(|| format!("tenant {} inconnu", id))?;
    for server in servers.keys().chain(shared.iter()) {
        if !known.contains(server) {
            return Err(format!(
                "serveur {} absent de config.json (ou desactive)",
                server
            ));
        }
    }
    let mut credentials = serde_json::Map::new();
    for (server, vars) in servers {
        let mut map = serde_json::Map::new();
        for var in vars {
            if !crate::vault::is_valid_name(var) {
                return Err(format!("variable invalide {}.{}", server, var));
            }
            if var.starts_with(crate::oauth::VAULT_PREFIX) {
                return Err(format!("{} : nom reserve aux jetons OAuth", var));
            }
            let secret = def
                .credentials
                .get(server)
                .and_then(|m| m.get(var))
                .cloned()
                .unwrap_or_else(|| var.clone());
            map.insert(
                var.clone(),
                json!(format!("{}{}/{}", VAULT_REF_PREFIX, id, secret)),
            );
        }
        credentials.insert(server.clone(), Value::Object(map));
    }
    let shared: BTreeSet<&String> = shared.iter().collect();
    let entry = candidate
        .get_mut("projects")
        .and_then(|p| p.get_mut(&def.project))
        .and_then(|e| e.as_object_mut())
        .ok_or_else(|| format!("projet {} introuvable", def.project))?;
    entry.insert("credentials".into(), Value::Object(credentials));
    entry.insert("sharedServers".into(), json!(shared));
    if let Some(keep) = entry.get_mut("keepEnv").and_then(|k| k.as_object_mut()) {
        keep.retain(|server, _| servers.contains_key(server));
    }
    parse_projects(&candidate)?;
    *json = candidate;
    Ok(())
}

/// Vérifie qu'une instance peut démarrer : variables couvertes, secrets présents, OAuth connecté.
/// Le message ne contient jamais de valeur.
pub fn check_server(tenant: &TenantDef, server: &str) -> Result<String, String> {
    let config = derive_from_vault(tenant, server)?;
    if !config.oauth {
        return Ok("ok".into());
    }
    let connected = crate::oauth::load(&instance_name(server, &tenant.id))
        .and_then(|s| s.tokens)
        .is_some();
    if connected {
        Ok("ok (OAuth connecte)".into())
    } else {
        Err(format!(
            "OAuth non connecte : McpHub oauth login --tenant {} {}",
            tenant.id, server
        ))
    }
}

/// Efface tout ce qui appartient au tenant (jetons OAuth, coffre, dossier et HOME isolé), puis
/// le retire de projects.json (le projet et ses dossiers restent). Renvoie le compte rendu.
/// Le coffre est effacé en premier : en cas d'échec, rien d'autre n'est touché.
/// `expected_version` (tableau de bord) : refus si projects.json a changé depuis l'affichage.
pub fn purge_tenant(
    path: &Path,
    id: &str,
    expected_version: Option<&str>,
) -> Result<Vec<String>, String> {
    if !is_valid_id(id) {
        return Err(format!("id invalide : {}", id));
    }
    let _guard = EDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if expected_version.is_some_and(|v| v != projects_version(path)) {
        return Err(STALE_PROJECTS.into());
    }
    let mut report = Vec::new();
    let mut json = read_projects_raw(path)?;
    if let Ok(tenants) = parse_projects(&json) {
        if let Some(def) = tenants.get(id) {
            for server in def.credentials.keys() {
                // Vide aussi le cache des jetons du processus courant (démon).
                if let Err(e) = crate::oauth::forget(&instance_name(server, id)) {
                    report.push(format!(
                        "Avertissement : jetons OAuth de {} : {}",
                        server, e
                    ));
                }
            }
        }
    }
    let scope = Scope::Tenant(id.to_string());
    let erased =
        crate::vault::purge(&scope).map_err(|e| format!("rien d'autre n'a ete touche : {}", e))?;
    report.push(format!(
        "Coffre du tenant {} efface ({} elements).",
        id, erased
    ));
    for dir in [tenant_dir(id), tenant_home(id)] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => report.push(format!("Supprime : {}", dir.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report.push(format!("Avertissement : {} : {}", dir.display(), e)),
        }
    }
    match remove_tenant(&mut json, id) {
        Ok(project) => {
            let backup = write_projects(path, &json, &format!("rm-tenant-{}", id))
                .map_err(|e| format!("ecriture de {} : {}", path.display(), e))?;
            report.push(format!("Tenant {} retire du projet {}.", id, project));
            if let Some(b) = backup {
                report.push(format!("Sauvegarde : {}", b.display()));
            }
        }
        Err(e) => report.push(format!("({} : {})", path.display(), e)),
    }
    Ok(report)
}

/// Fichier des projets de cf-orchestrator (dans le vrai dossier personnel, comme lui).
pub fn cf_orchestrator_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cf-orchestrator").join("projects.json"))
}

/// Projets cf-orchestrator rattachés au tenant (lecture seule) : nom, projet Cloudflare, type.
pub fn cf_orchestrator_projects(path: &Path, tenant: &str) -> Result<Vec<Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{} illisible : {}", path.display(), e)),
    };
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} : JSON invalide ({})", path.display(), e))?;
    let projects = json
        .get("projects")
        .and_then(|p| p.as_object())
        .ok_or_else(|| format!("{} : objet \"projects\" absent", path.display()))?;
    Ok(projects
        .iter()
        .filter(|(_, p)| p.get("tenant").and_then(|t| t.as_str()) == Some(tenant))
        .map(|(name, p)| {
            json!({
                "name": name,
                "cf_project_name": p.get("cf_project_name").and_then(|v| v.as_str()),
                "type": p.get("type").and_then(|v| v.as_str()),
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Retire `--tenant ID` (ou `--tenant=ID`) des arguments. Le tenant doit être déclaré.
pub fn take_tenant_flag(args: &[String]) -> Result<(Scope, Vec<String>), String> {
    let mut rest = Vec::new();
    let mut tenant: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--tenant" {
            tenant = Some(iter.next().cloned().ok_or("--tenant attend un id")?);
        } else if let Some(id) = arg.strip_prefix("--tenant=") {
            tenant = Some(id.to_string());
        } else {
            rest.push(arg.clone());
        }
    }
    let Some(id) = tenant else {
        return Ok((Scope::Hub, rest));
    };
    if !is_valid_id(&id) {
        return Err(format!("id de tenant invalide : {}", id));
    }
    let tenants = load_from(&projects_path())?;
    if tenants.get(&id).is_none() {
        return Err(format!(
            "tenant {} inconnu dans {} (McpHub tenant add {} --dir DOSSIER)",
            id,
            projects_path().display(),
            id
        ));
    }
    Ok((Scope::Tenant(id), rest))
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{}=", name);
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == name {
            return iter.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}

fn positional(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if matches!(a.as_str(), "--dir" | "--project") {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        out.push(a);
    }
    out
}

fn read_projects_raw(path: &Path) -> Result<Value, String> {
    Ok(crate::config::read_raw(path)?.unwrap_or_else(|| json!({ "projects": {} })))
}

/// Écrit projects.json après validation, avec une copie de l'ancien fichier.
fn write_projects(path: &Path, json: &Value, label: &str) -> Result<Option<PathBuf>, String> {
    parse_projects(json)?;
    let backup = if path.exists() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let backup = path.with_file_name(format!("{}.avant-{}-{}", file, label, stamp));
        std::fs::copy(path, &backup)
            .map_err(|e| format!("sauvegarde {} : {}", backup.display(), e))?;
        crate::vault::restrict_permissions(&backup);
        Some(backup)
    } else {
        None
    };
    crate::config::write_raw_atomic(path, json)?;
    Ok(backup)
}

/// Ajoute le tenant au projet (créé s'il n'existe pas). Rien ne change en cas d'erreur.
pub fn add_tenant(json: &mut Value, id: &str, dir: &Path, project: &str) -> Result<(), String> {
    let mut candidate = json.clone();
    add_tenant_to(&mut candidate, id, dir, project)?;
    *json = candidate;
    Ok(())
}

fn add_tenant_to(json: &mut Value, id: &str, dir: &Path, project: &str) -> Result<(), String> {
    if !is_valid_id(id) {
        return Err(format!(
            "id invalide : {} (1 a 32 minuscules, chiffres, tirets)",
            id
        ));
    }
    if !dir.is_absolute() {
        return Err(format!("dossier non absolu : {}", dir.display()));
    }
    let existing = parse_projects(json)?;
    if existing.get(id).is_some() {
        return Err(format!("le tenant {} existe deja", id));
    }
    let root = json
        .as_object_mut()
        .ok_or("projects.json : la racine doit etre un objet")?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("\"projects\" doit etre un objet")?;
    let dir_text = dir.to_string_lossy().into_owned();
    match projects.get_mut(project) {
        Some(entry) => {
            let obj = entry
                .as_object_mut()
                .ok_or_else(|| format!("projet {} invalide", project))?;
            if obj.contains_key("tenant") {
                return Err(format!("le projet {} a deja un tenant", project));
            }
            let workspaces = obj
                .entry("workspaces")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| format!("projet {} : workspaces invalide", project))?;
            if !workspaces
                .iter()
                .any(|w| w.as_str() == Some(dir_text.as_str()))
            {
                workspaces.push(json!(dir_text));
            }
            obj.insert("tenant".into(), json!(id));
            obj.entry("credentials").or_insert_with(|| json!({}));
            obj.entry("sharedServers").or_insert_with(|| json!([]));
        }
        None => {
            projects.insert(
                project.to_string(),
                json!({
                    "workspaces": [dir_text],
                    "aliases": {},
                    "tenant": id,
                    "credentials": {},
                    "sharedServers": []
                }),
            );
        }
    }
    parse_projects(json).map(|_| ())
}

/// Retire le tenant de son projet (le projet et ses dossiers restent).
pub fn remove_tenant(json: &mut Value, id: &str) -> Result<String, String> {
    let tenants = parse_projects(json)?;
    let project = tenants
        .get(id)
        .ok_or_else(|| format!("tenant {} inconnu", id))?
        .project
        .clone();
    let entry = json
        .get_mut("projects")
        .and_then(|p| p.get_mut(&project))
        .and_then(|e| e.as_object_mut())
        .ok_or_else(|| format!("projet {} introuvable", project))?;
    for key in ["tenant", "credentials", "keepEnv", "sharedServers"] {
        entry.remove(key);
    }
    Ok(project)
}

pub fn run_cli(args: &[String]) -> i32 {
    let path = projects_path();
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "add" => {
            let pos = positional(&args[1..]);
            let (Some(id), Some(dir)) = (pos.first(), flag_value(args, "--dir")) else {
                eprintln!("Usage : McpHub tenant add ID --dir DOSSIER [--project NOM]");
                return 2;
            };
            let dir = PathBuf::from(dir);
            if !dir.is_dir() {
                eprintln!("Dossier introuvable : {}", dir.display());
                return 1;
            }
            let project = flag_value(args, "--project").unwrap_or_else(|| id.to_string());
            let result = read_projects_raw(&path).and_then(|mut json| {
                add_tenant(&mut json, id, &normalize(&dir), &project)?;
                write_projects(&path, &json, &format!("tenant-{}", id))
            });
            match result {
                Ok(backup) => {
                    println!(
                        "Tenant {} ajoute au projet {} ({}).",
                        id,
                        project,
                        path.display()
                    );
                    if let Some(b) = backup {
                        println!("Sauvegarde : {}", b.display());
                    }
                    println!("Suite : declarer ses serveurs dans \"credentials\", puis McpHub vault set --tenant {} NOM", id);
                    0
                }
                Err(e) => {
                    eprintln!("Erreur : {}", e);
                    1
                }
            }
        }
        "list" | "ls" => match load_from(&path) {
            Ok(tenants) => {
                if tenants.tenants.is_empty() {
                    println!("Aucun tenant dans {}.", path.display());
                }
                for t in &tenants.tenants {
                    print_tenant(t);
                }
                0
            }
            Err(e) => {
                eprintln!("Erreur : {}", e);
                1
            }
        },
        "check" => {
            let Some(id) = positional(&args[1..]).first().map(|s| s.to_string()) else {
                eprintln!("Usage : McpHub tenant check ID");
                return 2;
            };
            let tenants = match load_from(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Erreur : {}", e);
                    return 1;
                }
            };
            let Some(tenant) = tenants.get(&id) else {
                eprintln!("Tenant {} inconnu.", id);
                return 1;
            };
            let mut failures = 0;
            for server in tenant.credentials.keys() {
                match check_server(tenant, server) {
                    Ok(message) => println!("  {:24} {}", server, message),
                    Err(e) => {
                        failures += 1;
                        println!("  {:24} ERREUR : {}", server, e);
                    }
                }
            }
            if failures == 0 {
                0
            } else {
                1
            }
        }
        "rm" | "remove" | "delete" => {
            let Some(id) = positional(&args[1..]).first().map(|s| s.to_string()) else {
                eprintln!("Usage : McpHub tenant rm ID [--yes]");
                return 2;
            };
            if !args.iter().any(|a| a == "--yes") && !confirm_removal(&id) {
                eprintln!("Annule.");
                return 1;
            }
            remove_cli(&path, &id)
        }
        _ => {
            eprintln!(
                r#"McpHub tenant : comptes clients isoles

  McpHub tenant add ID --dir DOSSIER [--project NOM]   Declare un tenant (ecrit projects.json, avec sauvegarde)
  McpHub tenant list                                   Tenants, dossiers, serveurs, secrets presents (jamais les valeurs)
  McpHub tenant check ID                               Verifie que chaque instance peut demarrer
  McpHub tenant rm ID [--yes]                          Efface ses secrets, jetons OAuth et HOME isole, puis le retire

Secrets : McpHub vault set --tenant ID NOM     OAuth : McpHub oauth login --tenant ID SERVEUR"#
            );
            2
        }
    }
}

fn print_tenant(t: &TenantDef) {
    let scope = Scope::Tenant(t.id.clone());
    let stored: BTreeSet<String> = crate::vault::list_names_in(&scope).into_iter().collect();
    println!("{} (projet {})", t.id, t.project);
    for w in &t.workspaces {
        println!("  dossier   {}", w.display());
    }
    for (server, vars) in &t.credentials {
        let oauth_stored = stored.contains(&crate::oauth::vault_name(server));
        let secrets: Vec<String> = vars
            .values()
            .map(|n| {
                format!(
                    "{}{}",
                    n,
                    if stored.contains(n) {
                        ""
                    } else {
                        " (MANQUANT)"
                    }
                )
            })
            .collect();
        println!(
            "  serveur   {:24} {}{}",
            server,
            if secrets.is_empty() {
                "-".to_string()
            } else {
                secrets.join(", ")
            },
            if oauth_stored { "  [OAuth]" } else { "" }
        );
    }
    if !t.shared_servers.is_empty() {
        println!(
            "  partages  {}",
            t.shared_servers
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn confirm_removal(id: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!(
        "Effacer TOUS les secrets et jetons OAuth du tenant {} (irreversible) ? Retaper l'id pour confirmer : ",
        id
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    line.trim() == id
}

fn remove_cli(path: &Path, id: &str) -> i32 {
    match purge_tenant(path, id, None) {
        Ok(report) => {
            for line in report {
                println!("{}", line);
            }
            0
        }
        Err(e) => {
            eprintln!("Erreur : {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects() -> Value {
        json!({
            "projects": {
                "soflution-app": {
                    "workspaces": ["/w/App/Sofapp"],
                    "allowedServers": ["*"]
                },
                "acme": {
                    "workspaces": ["/w/Clients/Acme"],
                    "tenant": "acme",
                    "credentials": {
                        "cloudflare-api": { "CLOUDFLARE_MCP_TOKEN": "vault:acme/CF_TOKEN" },
                        "supabase": { "SUPABASE_ACCESS_TOKEN": "vault:acme/SUPABASE_ACCESS_TOKEN" },
                        "stripe": {}
                    },
                    "keepEnv": { "supabase": ["NODE_OPTIONS"] },
                    "sharedServers": ["context7"]
                }
            }
        })
    }

    fn tenants() -> Tenants {
        parse_projects(&projects()).unwrap()
    }

    fn origin(wd: Option<&str>, cwd: Option<&str>) -> CallOrigin {
        CallOrigin {
            working_dir: wd.map(PathBuf::from),
            cwd: cwd.map(PathBuf::from),
        }
    }

    fn ctx(wd: Option<&str>, cwd: Option<&str>) -> Result<Context, String> {
        resolve_context(&tenants(), &origin(wd, cwd))
    }

    #[test]
    fn lit_les_tenants() {
        let t = tenants();
        assert_eq!(t.tenants.len(), 1);
        let acme = t.get("acme").unwrap();
        assert_eq!(acme.project, "acme");
        assert_eq!(
            acme.credentials["cloudflare-api"]["CLOUDFLARE_MCP_TOKEN"],
            "CF_TOKEN"
        );
        assert!(acme.credentials["stripe"].is_empty());
        assert!(acme.shared_servers.contains("context7"));
    }

    #[test]
    fn refuse_les_declarations_douteuses() {
        let cases = [
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "A B"}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "credentials": {}}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "a",
                "credentials": {"x": {"T": "vault:autre/T"}}}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "a",
                "credentials": {"x": {"T": "valeur-en-clair"}}}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "a",
                "credentials": {"x@y": {}}}}}),
            json!({"projects": {"a": {"workspaces": ["relatif"], "tenant": "a"}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "a",
                "credentials": {"x": {}}, "sharedServers": ["x"]}}}),
            json!({"projects": {"a": {"workspaces": ["/a"], "tenant": "a", "keepEnv": {"x": ["P"]}}}}),
            json!({"projects": {
                "a": {"workspaces": ["/c/a"], "tenant": "a"},
                "b": {"workspaces": ["/c/a/sous"], "tenant": "b"}}}),
            json!({"projects": {
                "a": {"workspaces": ["/a"], "tenant": "t"},
                "b": {"workspaces": ["/b"], "tenant": "t"}}}),
        ];
        for case in cases {
            assert!(
                parse_projects(&case).is_err(),
                "devait etre refuse : {}",
                case
            );
        }
    }

    #[test]
    fn contexte_selon_le_dossier() {
        assert_eq!(ctx(None, None).unwrap(), Context::Hub);
        assert_eq!(ctx(Some("/w/App/Sofapp"), None).unwrap(), Context::Hub);
        assert_eq!(
            ctx(Some("/w/Clients/Acme/site/src"), None)
                .unwrap()
                .tenant_id(),
            Some("acme")
        );
        // Préfixe de nom sans être le dossier : pas le tenant.
        assert_eq!(ctx(Some("/w/Clients/Acme2"), None).unwrap(), Context::Hub);
        // `..` ne fait pas sortir du dossier en apparence seulement.
        assert_eq!(
            ctx(Some("/w/Clients/Acme/../Autre"), None).unwrap(),
            Context::Hub
        );
        // Sans working_dir : le dossier courant du client.
        assert_eq!(
            ctx(None, Some("/w/Clients/Acme")).unwrap().tenant_id(),
            Some("acme")
        );
        assert_eq!(
            ctx(Some("/w/Clients/Acme"), Some("/w/Clients/Acme/x"))
                .unwrap()
                .tenant_id(),
            Some("acme")
        );
        // Un working_dir de tenant depuis un client lancé ailleurs reste le tenant.
        assert_eq!(
            ctx(Some("/w/Clients/Acme"), Some("/w/App/Sofapp"))
                .unwrap()
                .tenant_id(),
            Some("acme")
        );
    }

    #[cfg(unix)]
    #[test]
    fn lien_symbolique_et_casse_restent_dans_le_tenant() {
        let root = std::env::temp_dir().join(format!("tenant-lien-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("Clients").join("Acme");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("raccourci");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let json = json!({"projects": {"acme": {"workspaces": [real], "tenant": "acme"}}});
        let t = parse_projects(&json).unwrap();
        assert_eq!(
            t.for_dir(&link.join("site")).map(|d| d.id.as_str()),
            Some("acme")
        );
        if cfg!(target_os = "macos") {
            let other_case = PathBuf::from(real.to_string_lossy().to_uppercase());
            assert_eq!(t.for_dir(&other_case).map(|d| d.id.as_str()), Some("acme"));
        }
        assert!(t.for_dir(&root.join("Clients").join("Acme2")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn client_de_tenant_ne_vise_pas_un_autre_dossier() {
        let err = ctx(Some("/w/App/Sofapp"), Some("/w/Clients/Acme")).unwrap_err();
        assert!(err.contains("acme"), "{}", err);
    }

    #[test]
    fn tenant_ne_joint_jamais_les_serveurs_mcphub() {
        let acme = ctx(Some("/w/Clients/Acme"), None).unwrap();
        // Serveur du tenant : son instance.
        let r = route(&acme, "cloudflare-api").unwrap();
        assert_eq!(r.key(), "cloudflare-api@acme");
        assert!(matches!(&r, Route::Tenant { tenant, .. } if tenant == "acme"));
        assert_eq!(r.server(), "cloudflare-api");
        // Serveur partagé : instance normale.
        assert_eq!(
            route(&acme, "context7").unwrap(),
            Route::Hub("context7".into())
        );
        // Tout autre serveur : refusé, même s'il existe côté McpHub.
        assert!(route(&acme, "github").is_err());
        assert!(route(&acme, "app-store-connect").is_err());
    }

    #[test]
    fn mcphub_ne_joint_jamais_un_tenant() {
        let hub = ctx(Some("/w/App/Sofapp"), None).unwrap();
        assert_eq!(
            route(&hub, "cloudflare-api").unwrap(),
            Route::Hub("cloudflare-api".into())
        );
        assert!(route(&hub, "cloudflare-api@acme").is_err());
        // Sans aucun dossier connu non plus.
        assert_eq!(
            route(&Context::Hub, "supabase").unwrap(),
            Route::Hub("supabase".into())
        );
        assert!(route(&Context::Hub, "supabase@acme").is_err());
        // Ni depuis un tenant par le nom de l'instance.
        let acme = ctx(Some("/w/Clients/Acme"), None).unwrap();
        assert!(route(&acme, "supabase@acme").is_err());
    }

    #[test]
    fn recherche_limitee_au_perimetre() {
        let acme = ctx(Some("/w/Clients/Acme"), None).unwrap();
        assert!(visible(&acme, "supabase"));
        assert!(visible(&acme, "context7"));
        assert!(!visible(&acme, "github"));
        assert!(visible(&Context::Hub, "github"));
        assert!(!visible(&Context::Hub, "github@acme"));
    }

    fn vault(name: &str) -> Result<Option<String>, String> {
        Ok(match name {
            "CF_TOKEN" => Some("cf-acme".into()),
            "SUPABASE_ACCESS_TOKEN" => Some("sb-acme".into()),
            _ => None,
        })
    }

    #[test]
    fn instance_sans_aucune_variable_du_demon() {
        let acme = tenants().get("acme").unwrap().clone();
        let raw = json!({
            "command": "/usr/bin/env",
            "args": ["${SUPABASE_ACCESS_TOKEN}"],
            "env": {
                "PATH": "/opt/bin:/usr/bin",
                "SUPABASE_ACCESS_TOKEN": "${SUPABASE_ACCESS_TOKEN}",
                "NODE_OPTIONS": "--no-warnings",
                "APP_STORE_CONNECT_P8_PATH": "/Users/moi/.private_keys/AuthKey.p8",
                "AUTRE_SECRET": "${GITHUB_TOKEN}"
            },
            "inheritEnv": ["GITHUB_TOKEN"]
        });
        let cfg = derive_config(&acme, "supabase", &raw, None, &vault).unwrap();
        assert_eq!(cfg.tenant.as_deref(), Some("acme"));
        assert_eq!(cfg.env["SUPABASE_ACCESS_TOKEN"], "sb-acme");
        assert_eq!(cfg.args, vec!["sb-acme".to_string()]);
        assert_eq!(cfg.env["PATH"], "/opt/bin:/usr/bin");
        assert_eq!(cfg.env["NODE_OPTIONS"], "--no-warnings");
        // Valeur littérale non déclarée (clé .p8 McpHub) : jamais transmise.
        assert!(!cfg.env.contains_key("APP_STORE_CONNECT_P8_PATH"));
        assert!(!cfg.env.contains_key("AUTRE_SECRET"));
        assert!(cfg.inherit_env.is_empty());
        assert_eq!(cfg.env["MCPHUB_TENANT"], "acme");
        let home = tenant_home("acme");
        assert_eq!(cfg.env["HOME"], home.to_string_lossy());
        assert!(cfg.env["XDG_CONFIG_HOME"].starts_with(&*home.to_string_lossy()));
    }

    #[test]
    fn variable_non_couverte_bloque_le_demarrage() {
        let acme = tenants().get("acme").unwrap().clone();
        let raw = json!({
            "url": "https://mcp.example.com/mcp",
            "headers": { "Authorization": "Bearer ${CLOUDFLARE_MCP_TOKEN}", "X-Compte": "${CF_ACCOUNT_ID}" }
        });
        let err = derive_config(&acme, "cloudflare-api", &raw, None, &vault).unwrap_err();
        assert!(err.contains("CF_ACCOUNT_ID"), "{}", err);
        // La valeur du secret n'apparaît jamais dans l'erreur.
        assert!(!err.contains("cf-acme"), "{}", err);
    }

    #[test]
    fn serveur_distant_avec_le_jeton_du_tenant() {
        let acme = tenants().get("acme").unwrap().clone();
        let raw = json!({
            "url": "https://mcp.example.com/mcp",
            "auth": "none",
            "headers": { "Authorization": "Bearer ${CLOUDFLARE_MCP_TOKEN}" }
        });
        let cfg = derive_config(&acme, "cloudflare-api", &raw, None, &vault).unwrap();
        assert_eq!(cfg.headers["Authorization"], "Bearer cf-acme");
        assert!(!cfg.oauth);
        assert!(cfg.env.is_empty());
    }

    #[test]
    fn secret_manquant_explique() {
        let acme = tenants().get("acme").unwrap().clone();
        let raw = json!({ "url": "https://x/mcp", "headers": { "Authorization": "Bearer ${CLOUDFLARE_MCP_TOKEN}" } });
        let err = derive_config(&acme, "cloudflare-api", &raw, None, &|_| Ok(None)).unwrap_err();
        assert!(err.contains("vault set --tenant acme CF_TOKEN"), "{}", err);
        // Serveur hors du tenant.
        assert!(derive_config(&acme, "github", &raw, None, &vault).is_err());
    }

    #[test]
    fn noms_d_instance() {
        assert!(is_instance_name("stripe@acme"));
        assert!(!is_instance_name("stripe"));
        assert_eq!(split_instance("stripe@acme"), Some(("stripe", "acme")));
        assert!(is_valid_id("ideekdo"));
        assert!(!is_valid_id("Idee"));
        assert!(!is_valid_id("-a"));
    }

    #[test]
    fn ajout_et_retrait_dans_projects_json() {
        let mut json = projects();
        add_tenant(&mut json, "beta", Path::new("/w/Clients/Beta"), "beta").unwrap();
        let t = parse_projects(&json).unwrap();
        assert_eq!(
            t.get("beta").unwrap().workspaces,
            vec![PathBuf::from("/w/Clients/Beta")]
        );
        assert!(add_tenant(&mut json, "beta", Path::new("/w/x"), "x").is_err());
        assert!(add_tenant(&mut json, "gamma", Path::new("/w/Clients/Beta/sous"), "g").is_err());
        // Tenant sur un projet existant : ses dossiers et alias restent.
        add_tenant(
            &mut json,
            "sof",
            Path::new("/w/App/Sofapp"),
            "soflution-app",
        )
        .unwrap();
        assert_eq!(
            json["projects"]["soflution-app"]["allowedServers"],
            json!(["*"])
        );
        assert_eq!(remove_tenant(&mut json, "sof").unwrap(), "soflution-app");
        assert!(json["projects"]["soflution-app"].get("tenant").is_none());
        assert!(json["projects"]["soflution-app"]
            .get("workspaces")
            .is_some());
        assert!(parse_projects(&json).unwrap().get("sof").is_none());
    }

    #[test]
    fn serveurs_d_un_tenant_depuis_l_ecran() {
        let known: BTreeSet<String> =
            ["cloudflare-api", "supabase", "stripe", "context7", "svelte"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut json = projects();
        let servers: BTreeMap<String, Vec<String>> = [
            // Variable déjà déclarée : elle garde son secret (CF_TOKEN), la nouvelle prend son nom.
            (
                "cloudflare-api".to_string(),
                vec!["CLOUDFLARE_MCP_TOKEN".to_string(), "ACCOUNT_ID".to_string()],
            ),
            ("stripe".to_string(), vec![]),
        ]
        .into_iter()
        .collect();
        set_servers(
            &mut json,
            "acme",
            &servers,
            &["svelte".into(), "context7".into()],
            &known,
        )
        .unwrap();
        let entry = &json["projects"]["acme"];
        assert_eq!(
            entry["credentials"],
            json!({
                "cloudflare-api": { "CLOUDFLARE_MCP_TOKEN": "vault:acme/CF_TOKEN", "ACCOUNT_ID": "vault:acme/ACCOUNT_ID" },
                "stripe": {}
            })
        );
        assert_eq!(entry["sharedServers"], json!(["context7", "svelte"]));
        // keepEnv d'un serveur retiré disparaît avec lui.
        assert_eq!(entry["keepEnv"], json!({}));
        assert_eq!(
            json["projects"]["soflution-app"],
            projects()["projects"]["soflution-app"]
        );

        // Refus sans rien modifier : serveur inconnu, nom OAuth réservé, double déclaration.
        let before = json.clone();
        let unknown: BTreeMap<String, Vec<String>> =
            [("inconnu".to_string(), vec![])].into_iter().collect();
        assert!(set_servers(&mut json, "acme", &unknown, &[], &known).is_err());
        let oauth: BTreeMap<String, Vec<String>> = [(
            "stripe".to_string(),
            vec!["MCPHUB_OAUTH_STRIPE".to_string()],
        )]
        .into_iter()
        .collect();
        assert!(set_servers(&mut json, "acme", &oauth, &[], &known).is_err());
        let both: BTreeMap<String, Vec<String>> =
            [("stripe".to_string(), vec![])].into_iter().collect();
        assert!(set_servers(&mut json, "acme", &both, &["stripe".into()], &known).is_err());
        assert!(set_servers(&mut json, "inconnu", &both, &[], &known).is_err());
        assert_eq!(json, before);
    }

    #[test]
    fn variables_attendues_d_un_serveur() {
        let stdio = json!({
            "command": "npx",
            "args": ["-y", "pkg", "--account=${ACCOUNT_ID}"],
            "env": { "API_TOKEN": "${HUB_TOKEN}", "P8_PATH": "/cle.p8", "PATH": "/bin" }
        });
        let (required, optional) = server_variables(&stdio);
        assert_eq!(
            required.into_iter().collect::<Vec<_>>(),
            ["ACCOUNT_ID", "API_TOKEN"]
        );
        assert_eq!(optional.into_iter().collect::<Vec<_>>(), ["P8_PATH"]);
        let remote = json!({ "url": "https://x.example/mcp", "headers": { "Authorization": "Bearer ${X_TOKEN}" } });
        let (required, optional) = server_variables(&remote);
        assert_eq!(required.into_iter().collect::<Vec<_>>(), ["X_TOKEN"]);
        assert!(optional.is_empty());
    }

    #[test]
    fn ecriture_refusee_si_projects_json_a_change() {
        let dir = std::env::temp_dir().join(format!("tenant-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        std::fs::write(&path, projects().to_string()).unwrap();
        let version = projects_version(&path);
        // Modification à la main entre l'affichage et l'écriture.
        std::fs::write(&path, format!("{}\n", projects())).unwrap();
        let stale = edit_projects(&path, &version, "test", |_| Ok(()));
        assert_eq!(stale.unwrap_err(), STALE_PROJECTS);
        let fresh = projects_version(&path);
        let backup = edit_projects(&path, &fresh, "test", |json| {
            json["projects"]["acme"]["sharedServers"] = json!([]);
            Ok(())
        })
        .unwrap();
        assert!(backup.unwrap().exists());
        assert_ne!(projects_version(&path), fresh);
        // Une modification invalide n'est jamais écrite.
        let current = projects_version(&path);
        let invalid = edit_projects(&path, &current, "test", |json| {
            json["projects"]["acme"]["tenant"] = json!("A B");
            Ok(())
        });
        assert!(invalid.is_err());
        assert_eq!(projects_version(&path), current);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn projets_cf_orchestrator_du_tenant() {
        let dir = std::env::temp_dir().join(format!("tenant-cf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        assert!(cf_orchestrator_projects(&path, "acme").unwrap().is_empty());
        std::fs::write(
            &path,
            json!({ "projects": {
                "site": { "cf_project_name": "acme-site", "type": "pages", "tenant": "acme" },
                "hub": { "cf_project_name": "hub-site", "type": "workers" }
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            cf_orchestrator_projects(&path, "acme").unwrap(),
            vec![json!({ "name": "site", "cf_project_name": "acme-site", "type": "pages" })]
        );
        std::fs::write(&path, "pas du json").unwrap();
        assert!(cf_orchestrator_projects(&path, "acme").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn origine_lue_dans_les_arguments() {
        let o = CallOrigin::from_args(
            &json!({"_mcphub_context": {"working_dir": "/w/a", "cwd": "relatif"}}),
        );
        assert_eq!(o.working_dir, Some(PathBuf::from("/w/a")));
        assert_eq!(o.cwd, None, "chemin relatif ignore");
        assert!(CallOrigin::from_args(&json!({})).is_empty());
    }
}
