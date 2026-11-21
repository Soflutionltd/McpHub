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
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
        let dir = normalize(dir);
        self.tenants
            .iter()
            .flat_map(|t| t.workspaces.iter().map(move |w| (t, w)))
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

/// Nom de secret d'une référence `vault:<tenant>/<NOM>` : le tenant doit être le sien.
fn parse_vault_ref(tenant: &str, server: &str, var: &str, reference: &str) -> Result<String, String> {
    let rest = reference.strip_prefix(VAULT_REF_PREFIX).ok_or_else(|| {
        format!(
            "tenant {} : {}.{} doit valoir \"vault:{}/NOM\" (jamais une valeur en clair)",
            tenant, server, var, tenant
        )
    })?;
    let (owner, name) = rest.split_once('/').ok_or_else(|| {
        format!("tenant {} : {}.{} : reference \"{}\" sans /NOM", tenant, server, var, reference)
    })?;
    if owner != tenant {
        return Err(format!(
            "tenant {} : {}.{} pointe vers le coffre de \"{}\" : un tenant n'utilise que son propre coffre",
            tenant, server, var, owner
        ));
    }
    if !crate::vault::is_valid_name(name) {
        return Err(format!("tenant {} : nom de secret invalide \"{}\"", tenant, name));
    }
    Ok(name.to_string())
}

fn string_list(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(String::from).ok_or_else(|| format!("{} : liste de textes attendue", what)))
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
    let projects = projects.as_object().ok_or("\"projects\" doit etre un objet")?;
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
            .ok_or_else(|| format!("projet {} : \"tenant\" doit etre un id (1 a 32 minuscules, chiffres, tirets)", project))?
            .to_string();
        if tenants.iter().any(|t| t.id == id) {
            return Err(format!("tenant {} declare par deux projets", id));
        }
        let workspaces: Vec<PathBuf> = string_list(cfg.get("workspaces"), &format!("projet {} : workspaces", project))?
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if workspaces.is_empty() {
            return Err(format!("tenant {} : aucun dossier (workspaces)", id));
        }
        for w in &workspaces {
            if !w.is_absolute() {
                return Err(format!("tenant {} : dossier non absolu {}", id, w.display()));
            }
        }
        let mut credentials = BTreeMap::new();
        if let Some(creds) = cfg.get("credentials") {
            let creds = creds
                .as_object()
                .ok_or_else(|| format!("tenant {} : \"credentials\" doit etre un objet", id))?;
            for (server, vars) in creds {
                if is_instance_name(server) {
                    return Err(format!("tenant {} : nom de serveur invalide {}", id, server));
                }
                let vars = vars
                    .as_object()
                    .ok_or_else(|| format!("tenant {} : credentials.{} doit etre un objet", id, server))?;
                let mut map = BTreeMap::new();
                for (var, reference) in vars {
                    if !crate::vault::is_valid_name(var) {
                        return Err(format!("tenant {} : variable invalide {}.{}", id, server, var));
                    }
                    let reference = reference
                        .as_str()
                        .ok_or_else(|| format!("tenant {} : {}.{} doit etre un texte", id, server, var))?;
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
                    return Err(format!("tenant {} : keepEnv.{} sans credentials.{}", id, server, server));
                }
                keep_env.insert(server.clone(), string_list(Some(vars), &format!("tenant {} : keepEnv.{}", id, server))?);
            }
        }
        let shared_servers: BTreeSet<String> =
            string_list(cfg.get("sharedServers"), &format!("tenant {} : sharedServers", id))?
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
                    if wa.starts_with(wb) || wb.starts_with(wa) {
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
        Self { working_dir: dir("working_dir"), cwd: dir("cwd") }
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
    let from_wd = origin.working_dir.as_deref().and_then(|d| tenants.for_dir(d));
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
    Tenant { server: String, tenant: String, instance: String },
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
                !secrets.contains_key(k) && (keep.contains(k.as_str()) || ALWAYS_KEPT_ENV.contains(&k.as_str()))
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
        config.env.insert("HOME".into(), home.to_string_lossy().into_owned());
        config.env.insert("XDG_CONFIG_HOME".into(), home.join(".config").to_string_lossy().into_owned());
        config.env.insert("XDG_DATA_HOME".into(), home.join(".local/share").to_string_lossy().into_owned());
        config.env.insert("XDG_STATE_HOME".into(), home.join(".local/state").to_string_lossy().into_owned());
        config.env.insert("XDG_CACHE_HOME".into(), home.join(".cache").to_string_lossy().into_owned());
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
    let json = crate::config::read_raw(&path)?
        .ok_or_else(|| format!("{} absent : les tenants exigent une config McpHub dediee", path.display()))?;
    let servers = json.get("mcpServers").or_else(|| json.get("servers")).unwrap_or(&json);
    let raw = servers
        .get(server)
        .cloned()
        .ok_or_else(|| format!("serveur {} absent de {}", server, path.display()))?;
    if raw.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(format!("serveur {} desactive dans {}", server, path.display()));
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
        Self { path, cached: Mutex::new(None), derived_at: Mutex::new(HashMap::new()) }
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
        derived.get(instance).is_none_or(|at| at.elapsed() >= INSTANCE_CONFIG_TTL)
    }

    pub fn mark_derived(&self, instance: &str) {
        self.derived_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(instance.to_string(), Instant::now());
    }
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
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let backup = path.with_file_name(format!("{}.avant-{}-{}", file, label, stamp));
        std::fs::copy(path, &backup).map_err(|e| format!("sauvegarde {} : {}", backup.display(), e))?;
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
        return Err(format!("id invalide : {} (1 a 32 minuscules, chiffres, tirets)", id));
    }
    if !dir.is_absolute() {
        return Err(format!("dossier non absolu : {}", dir.display()));
    }
    let existing = parse_projects(json)?;
    if existing.get(id).is_some() {
        return Err(format!("le tenant {} existe deja", id));
    }
    let root = json.as_object_mut().ok_or("projects.json : la racine doit etre un objet")?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("\"projects\" doit etre un objet")?;
    let dir_text = dir.to_string_lossy().into_owned();
    match projects.get_mut(project) {
        Some(entry) => {
            let obj = entry.as_object_mut().ok_or_else(|| format!("projet {} invalide", project))?;
            if obj.contains_key("tenant") {
                return Err(format!("le projet {} a deja un tenant", project));
            }
            let workspaces = obj
                .entry("workspaces")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| format!("projet {} : workspaces invalide", project))?;
            if !workspaces.iter().any(|w| w.as_str() == Some(dir_text.as_str())) {
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
    let project = tenants.get(id).ok_or_else(|| format!("tenant {} inconnu", id))?.project.clone();
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
                    println!("Tenant {} ajoute au projet {} ({}).", id, project, path.display());
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
                match derive_from_vault(tenant, server) {
                    Ok(cfg) if cfg.oauth => {
                        let connected = crate::oauth::load(&instance_name(server, &id))
                            .and_then(|s| s.tokens)
                            .is_some();
                        if connected {
                            println!("  {:24} ok (OAuth connecte)", server);
                        } else {
                            failures += 1;
                            println!("  {:24} OAuth non connecte : McpHub oauth login --tenant {} {}", server, id, server);
                        }
                    }
                    Ok(_) => println!("  {:24} ok", server),
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
            .map(|n| format!("{}{}", n, if stored.contains(n) { "" } else { " (MANQUANT)" }))
            .collect();
        println!(
            "  serveur   {:24} {}{}",
            server,
            if secrets.is_empty() { "-".to_string() } else { secrets.join(", ") },
            if oauth_stored { "  [OAuth]" } else { "" }
        );
    }
    if !t.shared_servers.is_empty() {
        println!("  partages  {}", t.shared_servers.iter().cloned().collect::<Vec<_>>().join(", "));
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
    let mut json = match read_projects_raw(path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("Erreur : {}", e);
            return 1;
        }
    };
    if !is_valid_id(id) {
        eprintln!("id invalide : {}", id);
        return 1;
    }
    let scope = Scope::Tenant(id.to_string());
    match crate::vault::purge(&scope) {
        Ok(n) => println!("Coffre du tenant {} efface ({} elements).", id, n),
        Err(e) => {
            eprintln!("Erreur, rien d'autre n'a ete touche : {}", e);
            return 1;
        }
    }
    for dir in [tenant_dir(id), tenant_home(id)] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("Supprime : {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("Avertissement : {} : {}", dir.display(), e),
        }
    }
    match remove_tenant(&mut json, id) {
        Ok(project) => match write_projects(path, &json, &format!("rm-tenant-{}", id)) {
            Ok(backup) => {
                println!("Tenant {} retire du projet {}.", id, project);
                if let Some(b) = backup {
                    println!("Sauvegarde : {}", b.display());
                }
                0
            }
            Err(e) => {
                eprintln!("Erreur d'ecriture de {} : {}", path.display(), e);
                1
            }
        },
        Err(e) => {
            println!("({} : {})", path.display(), e);
            0
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
        CallOrigin { working_dir: wd.map(PathBuf::from), cwd: cwd.map(PathBuf::from) }
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
        assert_eq!(acme.credentials["cloudflare-api"]["CLOUDFLARE_MCP_TOKEN"], "CF_TOKEN");
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
            assert!(parse_projects(&case).is_err(), "devait etre refuse : {}", case);
        }
    }

    #[test]
    fn contexte_selon_le_dossier() {
        assert_eq!(ctx(None, None).unwrap(), Context::Hub);
        assert_eq!(ctx(Some("/w/App/Sofapp"), None).unwrap(), Context::Hub);
        assert_eq!(ctx(Some("/w/Clients/Acme/site/src"), None).unwrap().tenant_id(), Some("acme"));
        // Préfixe de nom sans être le dossier : pas le tenant.
        assert_eq!(ctx(Some("/w/Clients/Acme2"), None).unwrap(), Context::Hub);
        // `..` ne fait pas sortir du dossier en apparence seulement.
        assert_eq!(ctx(Some("/w/Clients/Acme/../Autre"), None).unwrap(), Context::Hub);
        // Sans working_dir : le dossier courant du client.
        assert_eq!(ctx(None, Some("/w/Clients/Acme")).unwrap().tenant_id(), Some("acme"));
        assert_eq!(ctx(Some("/w/Clients/Acme"), Some("/w/Clients/Acme/x")).unwrap().tenant_id(), Some("acme"));
        // Un working_dir de tenant depuis un client lancé ailleurs reste le tenant.
        assert_eq!(ctx(Some("/w/Clients/Acme"), Some("/w/App/Sofapp")).unwrap().tenant_id(), Some("acme"));
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
        assert_eq!(route(&acme, "context7").unwrap(), Route::Hub("context7".into()));
        // Tout autre serveur : refusé, même s'il existe côté McpHub.
        assert!(route(&acme, "github").is_err());
        assert!(route(&acme, "app-store-connect").is_err());
    }

    #[test]
    fn mcphub_ne_joint_jamais_un_tenant() {
        let hub = ctx(Some("/w/App/Sofapp"), None).unwrap();
        assert_eq!(route(&hub, "cloudflare-api").unwrap(), Route::Hub("cloudflare-api".into()));
        assert!(route(&hub, "cloudflare-api@acme").is_err());
        // Sans aucun dossier connu non plus.
        assert_eq!(route(&Context::Hub, "supabase").unwrap(), Route::Hub("supabase".into()));
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
        assert_eq!(t.get("beta").unwrap().workspaces, vec![PathBuf::from("/w/Clients/Beta")]);
        assert!(add_tenant(&mut json, "beta", Path::new("/w/x"), "x").is_err());
        assert!(add_tenant(&mut json, "gamma", Path::new("/w/Clients/Beta/sous"), "g").is_err());
        // Tenant sur un projet existant : ses dossiers et alias restent.
        add_tenant(&mut json, "sof", Path::new("/w/App/Sofapp"), "soflution-app").unwrap();
        assert_eq!(json["projects"]["soflution-app"]["allowedServers"], json!(["*"]));
        assert_eq!(remove_tenant(&mut json, "sof").unwrap(), "soflution-app");
        assert!(json["projects"]["soflution-app"].get("tenant").is_none());
        assert!(json["projects"]["soflution-app"].get("workspaces").is_some());
        assert!(parse_projects(&json).unwrap().get("sof").is_none());
    }

    #[test]
    fn origine_lue_dans_les_arguments() {
        let o = CallOrigin::from_args(&json!({"_mcphub_context": {"working_dir": "/w/a", "cwd": "relatif"}}));
        assert_eq!(o.working_dir, Some(PathBuf::from("/w/a")));
        assert_eq!(o.cwd, None, "chemin relatif ignore");
        assert!(CallOrigin::from_args(&json!({})).is_empty());
    }
}
