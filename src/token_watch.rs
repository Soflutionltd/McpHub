//! Surveillance de l'échéance des jetons, pour le compte McpHub et chaque compte client.
//!
//! - Une fiche par jeton (`token-meta.json`, jamais de valeur) : service, échéance donnée par le
//!   service ou saisie à l'enregistrement, état de la dernière vérification, serveurs qui
//!   l'utilisent.
//! - Contrôle régulier par le démon : chaque jeton connu d'un service est revérifié auprès de
//!   son API (un jeton révoqué est repéré même sans échéance).
//! - Alertes : notification macOS et bandeau du tableau de bord, 14 jours puis 3 jours avant,
//!   puis le jour même ; chaque palier n'est notifié qu'une fois.
//! - Avis aux agents : la réponse MCP d'un serveur dit en clair quel jeton a expiré ou expire
//!   bientôt, de quel compte, et où le renouveler.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::vault::Scope;

const DAY_MS: u64 = 86_400_000;
/// Premier palier d'alerte, en jours.
pub const WARN_DAYS: i64 = 14;
/// Second palier d'alerte, en jours.
pub const URGENT_DAYS: i64 = 3;
/// Intervalle entre deux contrôles par le démon.
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(12 * 3600);

/// Fiche d'un jeton. Aucune valeur secrète.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenMeta {
    /// Variable que le jeton fournit aux serveurs.
    pub var: String,
    /// Service (connecteur `cloudflare`, `github`, `supabase`, ou `oauth`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Échéance en millisecondes depuis l'époque Unix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// `service` (date donnée par le service) ou `manual` (saisie à l'enregistrement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_source: Option<String>,
    /// `ok`, `refused` (le service refuse le jeton) ou `reconnect` (OAuth à refaire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// Serveurs qui reçoivent ce jeton.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
    /// Dernier palier notifié (`14`, `3` ou `0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<String>,
}

pub type MetaFile = BTreeMap<String, TokenMeta>;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn meta_path(scope: &Scope) -> PathBuf {
    match scope {
        Scope::Hub => crate::instance::hub_dir().join("token-meta.json"),
        Scope::Tenant(id) => crate::tenant::tenant_dir(id).join("token-meta.json"),
    }
}

/// Écritures des fiches sérialisées dans le processus (contrôle et enregistrement d'un jeton).
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn load(scope: &Scope) -> MetaFile {
    std::fs::read_to_string(meta_path(scope))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save(scope: &Scope, meta: &MetaFile) -> Result<(), String> {
    let path = meta_path(scope);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    crate::vault::restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Modifie les fiches d'un coffre sous verrou.
pub fn update(scope: &Scope, edit: impl FnOnce(&mut MetaFile)) -> Result<(), String> {
    let _guard = write_lock();
    let mut meta = load(scope);
    edit(&mut meta);
    save(scope, &meta)
}

// ─── Dates ───────────────────────────────────────────────────

/// Jours depuis le 1970-01-01 pour une date du calendrier grégorien.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn num(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// Date et heure d'un service : `2026-12-31T23:59:59Z` (Cloudflare), `2026-12-31 23:59:59 UTC`
/// ou `2026-12-31 23:59:59 +0100` (GitHub), ou une date seule `2026-12-31` (fin de journée UTC).
pub fn parse_datetime(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let date = raw.get(..10)?;
    let (y, m, d) = (
        num(date.get(..4)?)?,
        num(date.get(5..7)?)?,
        num(date.get(8..10)?)?,
    );
    if date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
    {
        return None;
    }
    let rest = raw[10..].trim_start_matches(['T', ' ']);
    let (seconds, zone) = if rest.is_empty() {
        (86_399, "")
    } else {
        let time = rest.get(..8)?;
        let (h, mi, s) = (
            num(time.get(..2)?)?,
            num(time.get(3..5)?)?,
            num(time.get(6..8)?)?,
        );
        if h > 23 || mi > 59 || s > 60 {
            return None;
        }
        // Fractions de seconde ignorées (`.123Z`).
        let zone = rest[8..]
            .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit())
            .trim();
        (h * 3600 + mi * 60 + s, zone)
    };
    let offset = match zone {
        "" | "Z" | "UTC" | "GMT" => 0,
        z if z.len() >= 5 && (z.starts_with('+') || z.starts_with('-')) => {
            let digits: String = z[1..].chars().filter(|c| *c != ':').collect();
            let (oh, om) = (num(digits.get(..2)?)?, num(digits.get(2..4)?)?);
            let sign = if z.starts_with('-') { -1 } else { 1 };
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, m, d) * 86_400 + seconds - offset;
    (secs >= 0).then_some(secs as u64 * 1000)
}

/// Date lisible `AAAA-MM-JJ` (UTC).
pub fn format_date(ms: u64) -> String {
    let (y, m, d) = civil_from_days((ms / DAY_MS) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

// ─── Alertes ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Level {
    /// Le service refuse le jeton (expiré ou révoqué).
    Refused,
    /// Connexion OAuth expirée sans renouvellement possible.
    Reconnect,
    /// Échéance passée.
    Expired,
    /// Échéance dans `n` jours (au plus `WARN_DAYS`).
    Soon(i64),
}

impl Level {
    fn bucket(&self) -> &'static str {
        match self {
            Level::Soon(days) if *days > URGENT_DAYS => "14",
            Level::Soon(_) => "3",
            _ => "0",
        }
    }

    fn is_problem(&self) -> bool {
        !matches!(self, Level::Soon(_))
    }
}

/// État d'alerte d'un jeton, s'il y en a un.
pub fn level(meta: &TokenMeta, now: u64) -> Option<Level> {
    match meta.status.as_deref() {
        Some("refused") => return Some(Level::Refused),
        Some("reconnect") => return Some(Level::Reconnect),
        _ => {}
    }
    let expires = meta.expires_at?;
    if expires <= now {
        return Some(Level::Expired);
    }
    let days = (expires - now).div_ceil(DAY_MS) as i64;
    (days <= WARN_DAYS).then_some(Level::Soon(days))
}

fn service_label(meta: &TokenMeta) -> String {
    match meta.service.as_deref() {
        Some("oauth") => "OAuth".into(),
        Some(id) => crate::connectors::KEY_CONNECTORS
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.label.to_string())
            .unwrap_or_else(|| id.to_string()),
        None => "service".into(),
    }
}

fn account_label(tenant: Option<&str>) -> String {
    match tenant {
        Some(t) => format!("client {}", t),
        None => "compte McpHub".into(),
    }
}

fn state_text(level: &Level, meta: &TokenMeta) -> String {
    match level {
        Level::Refused => format!("refusé par {} (expiré ou révoqué)", service_label(meta)),
        Level::Reconnect => "connexion expirée, à refaire".into(),
        Level::Expired => format!(
            "expiré le {}",
            meta.expires_at.map(format_date).unwrap_or_default()
        ),
        Level::Soon(1) => "expire demain".into(),
        Level::Soon(days) => format!(
            "expire dans {} jours ({})",
            days,
            meta.expires_at.map(format_date).unwrap_or_default()
        ),
    }
}

/// Où renouveler, en clair pour un humain comme pour un agent.
fn renew_hint(tenant: Option<&str>, name: &str, meta: &TokenMeta) -> String {
    let server = meta.servers.first().map(String::as_str).unwrap_or(name);
    match (tenant, meta.service.as_deref()) {
        (None, Some("oauth")) => format!(
            "tableau de bord McpHub, onglet Servers, carte {}, bouton Reconnect (ou `McpHub oauth login {}`)",
            server, server
        ),
        (Some(t), Some("oauth")) => {
            format!("tableau de bord McpHub, onglet Client accounts, {}, serveur {}, bouton Reconnect", t, server)
        }
        (None, _) => format!("tableau de bord McpHub, onglet Servers, carte {}, bouton Replace", server),
        (Some(t), _) => format!("tableau de bord McpHub, onglet Client accounts, {}, secret {}, bouton Replace", t, name),
    }
}

/// Une alerte, pour le bandeau et les notifications.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub tenant: Option<String>,
    pub name: String,
    pub meta: TokenMeta,
    pub level: Level,
}

impl Alert {
    pub fn message(&self) -> String {
        format!(
            "Jeton {} {} ({}) : {}.",
            service_label(&self.meta),
            self.name,
            account_label(self.tenant.as_deref()),
            state_text(&self.level, &self.meta)
        )
    }

    pub fn to_json(&self) -> Value {
        let (level, days) = match self.level {
            Level::Refused => ("refused", None),
            Level::Reconnect => ("reconnect", None),
            Level::Expired => ("expired", None),
            Level::Soon(d) => ("soon", Some(d)),
        };
        json!({
            "tenant": self.tenant,
            "name": self.name,
            "var": self.meta.var,
            "service": self.meta.service,
            "servers": self.meta.servers,
            "level": level,
            "days_left": days,
            "expires_at": self.meta.expires_at,
            "message": self.message(),
            "connector": crate::connectors::describe(&self.meta.var, self.tenant.as_deref()),
        })
    }
}

fn tenant_ids() -> Vec<String> {
    crate::tenant::load_from(&crate::tenant::projects_path())
        .map(|t| t.tenants.into_iter().map(|t| t.id).collect())
        .unwrap_or_default()
}

fn scopes() -> Vec<Scope> {
    std::iter::once(Scope::Hub)
        .chain(tenant_ids().into_iter().map(Scope::Tenant))
        .collect()
}

fn scope_tenant(scope: &Scope) -> Option<String> {
    match scope {
        Scope::Hub => None,
        Scope::Tenant(id) => Some(id.clone()),
    }
}

/// Alertes de tous les coffres (lecture des fiches seulement).
pub fn alerts() -> Vec<Alert> {
    let now = now_ms();
    scopes()
        .into_iter()
        .flat_map(|scope| {
            let tenant = scope_tenant(&scope);
            load(&scope).into_iter().filter_map(move |(name, meta)| {
                level(&meta, now).map(|level| Alert {
                    tenant: tenant.clone(),
                    name,
                    meta,
                    level,
                })
            })
        })
        .collect()
}

// ─── Avis aux agents ─────────────────────────────────────────

/// Dernier jour où un serveur a reçu l'avis « expire bientôt » (une fois par jour).
fn notes_given() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
    static N: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    N.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Texte de l'avis pour un ensemble de fiches et un appel (réussi ou non).
fn note_from(
    tenant: Option<&str>,
    entries: &[(String, TokenMeta)],
    failed: bool,
    now: u64,
) -> Option<String> {
    let lines: Vec<String> = entries
        .iter()
        .filter_map(|(name, meta)| {
            let level = level(meta, now)?;
            // Après un appel réussi, une fiche « refusé » ou « expiré » est périmée (jeton
            // renouvelé hors du tableau de bord) : on ne l'annonce pas.
            if level.is_problem() != failed {
                return None;
            }
            Some(format!(
                "jeton {} {} ({}) : {}. Renouvellement : {}.",
                service_label(meta),
                name,
                account_label(tenant),
                state_text(&level, meta),
                renew_hint(tenant, name, meta)
            ))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let lead = if failed {
        "[McpHub] Cet échec vient probablement d'un jeton expiré : dire à l'utilisateur"
    } else {
        "[McpHub] À signaler à l'utilisateur"
    };
    Some(format!("{} : {}", lead, lines.join(" ")))
}

/// Avis à joindre à la réponse d'un serveur (`serveur` ou `serveur@tenant`). Échec : jeton
/// refusé ou expiré, à chaque appel. Réussite : échéance proche, une fois par jour et par serveur.
pub fn note_for(process_key: &str, failed: bool) -> Option<String> {
    let (server, scope) = match crate::tenant::split_instance(process_key) {
        Some((base, tenant)) => (base, Scope::Tenant(tenant.to_string())),
        None => (process_key, Scope::Hub),
    };
    let now = now_ms();
    let today = now / DAY_MS;
    if !failed && notes_given().get(process_key) == Some(&today) {
        return None;
    }
    let entries: Vec<(String, TokenMeta)> = load(&scope)
        .into_iter()
        .filter(|(_, m)| m.servers.iter().any(|s| s == server))
        .collect();
    if entries.is_empty() {
        return None;
    }
    if !failed {
        let note = note_from(scope_tenant(&scope).as_deref(), &entries, false, now)?;
        notes_given().insert(process_key.to_string(), today);
        return Some(note);
    }
    note_from(scope_tenant(&scope).as_deref(), &entries, true, now)
}

/// Message clair d'un refus d'authentification d'un serveur distant.
pub fn auth_failure_message(process_key: &str, oauth: bool) -> String {
    let (server, tenant) = match crate::tenant::split_instance(process_key) {
        Some((base, tenant)) => (base, Some(tenant)),
        None => (process_key, None),
    };
    if let Some(note) = note_for(process_key, true) {
        return note;
    }
    let where_ = match (tenant, oauth) {
        (None, true) => format!(
            "tableau de bord McpHub, onglet Servers, carte {}, bouton Connect (ou `McpHub oauth login {}`)",
            server, server
        ),
        (Some(t), true) => format!("tableau de bord McpHub, onglet Client accounts, {}, serveur {}, bouton Connect", t, server),
        (None, false) => format!("tableau de bord McpHub, onglet Servers, carte {}", server),
        (Some(t), false) => format!("tableau de bord McpHub, onglet Client accounts, {}, serveur {}", t, server),
    };
    format!(
        "[McpHub] {} ({}) refuse l'authentification : {} probablement expiré ou révoqué. Dire à l'utilisateur de le renouveler : {}.",
        server,
        account_label(tenant),
        if oauth { "la connexion OAuth a" } else { "le jeton a" },
        where_
    )
}

// ─── Contrôle régulier ───────────────────────────────────────

/// Jeton d'un coffre à surveiller : nom, variable, serveurs qui le reçoivent.
struct Watched {
    name: String,
    var: String,
    servers: BTreeSet<String>,
}

/// Jetons présents dans un coffre et utilisés par un serveur, avec la liste de tous les noms
/// présents. Erreur si la config ne se lit pas : le coffre est alors laissé tel quel.
fn watched(scope: &Scope) -> Result<(Vec<Watched>, BTreeSet<String>), String> {
    let present: BTreeSet<String> = crate::vault::list_names_in(scope).into_iter().collect();
    let mut by_name: BTreeMap<String, Watched> = BTreeMap::new();
    let mut add = |name: &str, var: &str, server: &str| {
        if !present.contains(name) {
            return;
        }
        by_name
            .entry(name.to_string())
            .or_insert_with(|| Watched {
                name: name.to_string(),
                var: var.to_string(),
                servers: BTreeSet::new(),
            })
            .servers
            .insert(server.to_string());
    };
    match scope {
        Scope::Hub => {
            for (server, raw) in crate::tenant::configured_servers()? {
                for var in crate::tenant::referenced_vars(&raw) {
                    add(&var, &var, &server);
                }
                add(&crate::oauth::vault_name(&server), &server, &server);
            }
        }
        Scope::Tenant(id) => {
            let tenants = crate::tenant::load_from(&crate::tenant::projects_path())?;
            if let Some(def) = tenants.get(id) {
                for (server, vars) in &def.credentials {
                    for (var, name) in vars {
                        add(name, var, server);
                    }
                    add(&crate::oauth::vault_name(server), server, server);
                }
            }
        }
    }
    Ok((by_name.into_values().collect(), present))
}

/// Résultat d'une vérification, appliqué à la fiche.
enum Check {
    Verified(Option<u64>),
    Refused,
    Reconnect,
    Unknown,
}

async fn check_one(scope: &Scope, w: &Watched) -> (Option<String>, Check) {
    if w.name.starts_with(crate::oauth::VAULT_PREFIX) {
        // Connexion OAuth : seule une connexion expirée sans jeton de renouvellement est à refaire.
        let instance = match scope {
            Scope::Hub => w.var.clone(),
            Scope::Tenant(id) => crate::tenant::instance_name(&w.var, id),
        };
        let stored = tokio::task::spawn_blocking(move || crate::oauth::load(&instance))
            .await
            .ok()
            .flatten();
        let dead = stored.and_then(|s| s.tokens).is_some_and(|t| {
            t.refresh_token.is_none() && t.expires_at.is_some_and(|e| e <= now_ms())
        });
        return (
            Some("oauth".into()),
            if dead {
                Check::Reconnect
            } else {
                Check::Unknown
            },
        );
    }
    let Some(connector) = crate::connectors::for_var(&w.var) else {
        return (None, Check::Unknown);
    };
    let (task_scope, name) = (scope.clone(), w.name.clone());
    let value = tokio::task::spawn_blocking(move || crate::vault::get_in(&task_scope, &name)).await;
    let Ok(Ok(Some(value))) = value else {
        return (Some(connector.id.into()), Check::Unknown);
    };
    let check = match crate::connectors::verify(connector, &value).await {
        Ok(v) => Check::Verified(v.expires_at),
        Err(crate::connectors::VerifyError::Refused(_)) => Check::Refused,
        Err(crate::connectors::VerifyError::Unreachable(_)) => Check::Unknown,
    };
    (Some(connector.id.into()), check)
}

/// Contrôle tous les coffres, met les fiches à jour et notifie les paliers franchis.
pub async fn check_all() {
    for scope in tokio::task::spawn_blocking(scopes)
        .await
        .unwrap_or_default()
    {
        let task_scope = scope.clone();
        let (list, present) = match tokio::task::spawn_blocking(move || watched(&task_scope)).await
        {
            Ok(Ok(found)) => found,
            Ok(Err(e)) => {
                eprintln!("[McpHub][TOKENS] controle saute (config illisible) : {}", e);
                continue;
            }
            Err(_) => continue,
        };
        let mut results = Vec::new();
        for w in &list {
            results.push((w, check_one(&scope, w).await));
        }
        let tenant = scope_tenant(&scope);
        let mut notify: Vec<String> = Vec::new();
        let saved = update(&scope, |meta| {
            // Jetons retirés du coffre : leur fiche n'a plus lieu d'être (les autres restent,
            // même sans serveur qui les utilise).
            meta.retain(|name, _| present.contains(name));
            for (w, (service, check)) in results {
                let entry = meta.entry(w.name.clone()).or_default();
                entry.var = w.var.clone();
                entry.servers = w.servers.iter().cloned().collect();
                if service.is_some() {
                    entry.service = service;
                }
                match check {
                    Check::Verified(expires) => {
                        entry.status = Some("ok".into());
                        entry.checked_at = Some(now_ms());
                        if expires.is_some() {
                            entry.expires_at = expires;
                            entry.expires_source = Some("service".into());
                        } else if entry.expires_source.as_deref() == Some("service") {
                            // Le service n'annonce plus d'échéance : l'ancienne date est périmée.
                            entry.expires_at = None;
                            entry.expires_source = None;
                        }
                    }
                    Check::Refused => {
                        entry.status = Some("refused".into());
                        entry.checked_at = Some(now_ms());
                    }
                    Check::Reconnect => entry.status = Some("reconnect".into()),
                    Check::Unknown => {
                        if entry.status.as_deref() == Some("reconnect") {
                            entry.status = None;
                        }
                    }
                }
                match level(entry, now_ms()) {
                    Some(l) => {
                        if entry.notified.as_deref() != Some(l.bucket()) {
                            entry.notified = Some(l.bucket().into());
                            let alert = Alert {
                                tenant: tenant.clone(),
                                name: w.name.clone(),
                                meta: entry.clone(),
                                level: l,
                            };
                            notify.push(alert.message());
                        }
                    }
                    None => entry.notified = None,
                }
            }
        });
        if let Err(e) = saved {
            eprintln!("[McpHub][TOKENS] fiches non enregistrees : {}", e);
        }
        for message in notify {
            crate::health::send_notification("McpHub : jeton à renouveler", &message);
        }
    }
}

/// Boucle du démon : premier contrôle peu après le démarrage, puis toutes les 12 heures.
pub async fn run_loop() {
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    loop {
        check_all().await;
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// Fiche d'un jeton qui vient d'être enregistré depuis le tableau de bord.
pub struct Recorded<'a> {
    pub name: &'a str,
    pub var: &'a str,
    pub service: Option<&'a str>,
    pub servers: Vec<String>,
    pub verified: bool,
    pub service_expiry: Option<u64>,
    pub manual_expiry: Option<u64>,
}

pub fn record(scope: &Scope, r: Recorded<'_>) -> Result<(), String> {
    update(scope, |meta| apply_record(meta, r))
}

fn apply_record(meta: &mut MetaFile, r: Recorded<'_>) {
    {
        let entry = meta.entry(r.name.to_string()).or_default();
        entry.var = r.var.to_string();
        entry.service = r.service.map(String::from);
        if !r.servers.is_empty() {
            entry.servers = r.servers;
        }
        entry.status = r.verified.then(|| "ok".to_string());
        entry.checked_at = r.verified.then(now_ms);
        // La date du service prime sur la saisie ; un nouveau jeton efface l'ancienne échéance.
        (entry.expires_at, entry.expires_source) = match (r.service_expiry, r.manual_expiry) {
            (Some(e), _) => (Some(e), Some("service".into())),
            (None, Some(e)) => (Some(e), Some("manual".into())),
            (None, None) => (None, None),
        };
        entry.notified = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_des_services() {
        assert_eq!(parse_datetime("1970-01-01T00:00:00Z"), Some(0));
        let cf = parse_datetime("2026-12-31T23:59:59Z").unwrap();
        assert_eq!(format_date(cf), "2026-12-31");
        assert_eq!(parse_datetime("2026-12-31 23:59:59 UTC"), Some(cf));
        assert_eq!(parse_datetime("2027-01-01 00:59:59 +0100"), Some(cf));
        assert_eq!(parse_datetime("2026-12-31T22:59:59-01:00"), Some(cf));
        assert_eq!(parse_datetime("2026-12-31T23:59:59.123Z"), Some(cf));
        assert_eq!(parse_datetime("2026-12-31"), Some(cf));
        assert_eq!(parse_datetime("2026-13-01"), None);
        assert_eq!(parse_datetime("demain"), None);
        assert_eq!(parse_datetime("2026-12-31 23:59:59 PST"), None);
    }

    fn meta(expires_in_days: Option<i64>, status: Option<&str>) -> TokenMeta {
        TokenMeta {
            var: "CLOUDFLARE_API_TOKEN".into(),
            service: Some("cloudflare".into()),
            expires_at: expires_in_days
                .map(|d| (1_000 * DAY_MS as i64 + d * DAY_MS as i64 - 1000) as u64),
            status: status.map(String::from),
            servers: vec!["cf-orchestrator".into()],
            ..Default::default()
        }
    }

    #[test]
    fn paliers_d_alerte() {
        let now = 1_000 * DAY_MS;
        assert_eq!(level(&meta(None, None), now), None);
        assert_eq!(level(&meta(Some(30), None), now), None);
        assert_eq!(level(&meta(Some(14), None), now), Some(Level::Soon(14)));
        assert_eq!(level(&meta(Some(3), None), now), Some(Level::Soon(3)));
        assert_eq!(level(&meta(Some(0), None), now), Some(Level::Expired));
        assert_eq!(
            level(&meta(Some(30), Some("refused")), now),
            Some(Level::Refused)
        );
        assert_eq!(Level::Soon(10).bucket(), "14");
        assert_eq!(Level::Soon(2).bucket(), "3");
        assert_eq!(Level::Expired.bucket(), "0");
    }

    #[test]
    fn avis_aux_agents() {
        let now = 1_000 * DAY_MS;
        let soon = vec![("CLOUDFLARE_API_TOKEN".to_string(), meta(Some(2), None))];
        let note = note_from(Some("acme"), &soon, false, now).unwrap();
        assert!(
            note.contains("client acme") && note.contains("expire dans 2 jours"),
            "{}",
            note
        );
        assert!(
            note.contains("Client accounts, acme, secret CLOUDFLARE_API_TOKEN"),
            "{}",
            note
        );
        // Un appel en échec n'annonce pas une simple échéance proche comme cause.
        assert_eq!(note_from(Some("acme"), &soon, true, now), None);

        let expired = vec![("CLOUDFLARE_API_TOKEN".to_string(), meta(Some(-1), None))];
        let note = note_from(None, &expired, true, now).unwrap();
        assert!(
            note.contains("jeton expiré")
                && note.contains("compte McpHub")
                && note.contains("carte cf-orchestrator")
        );
        // Appel réussi malgré une fiche « expiré » : jeton déjà renouvelé, rien à dire.
        assert_eq!(note_from(None, &expired, false, now), None);
    }

    #[test]
    fn fiche_d_un_jeton_enregistre() {
        let recorded = |service_expiry, manual_expiry| Recorded {
            name: "GITHUB_TOKEN",
            var: "GITHUB_TOKEN",
            service: Some("github"),
            servers: vec!["github".into()],
            verified: true,
            service_expiry,
            manual_expiry,
        };
        let mut file = MetaFile::new();
        file.insert(
            "GITHUB_TOKEN".into(),
            TokenMeta {
                var: "GITHUB_TOKEN".into(),
                expires_at: Some(5),
                notified: Some("3".into()),
                ..Default::default()
            },
        );
        // La date donnée par le service prime sur la saisie.
        apply_record(&mut file, recorded(Some(100), Some(50)));
        let entry = &file["GITHUB_TOKEN"];
        assert_eq!(
            (entry.expires_at, entry.expires_source.as_deref()),
            (Some(100), Some("service"))
        );
        assert_eq!(
            (entry.notified.as_deref(), entry.status.as_deref()),
            (None, Some("ok"))
        );
        apply_record(&mut file, recorded(None, Some(50)));
        assert_eq!(
            file["GITHUB_TOKEN"].expires_source.as_deref(),
            Some("manual")
        );
        // Nouveau jeton sans échéance connue : l'ancienne date ne survit pas.
        apply_record(&mut file, recorded(None, None));
        assert_eq!(file["GITHUB_TOKEN"].expires_at, None);
        assert_eq!(file["GITHUB_TOKEN"].servers, vec!["github".to_string()]);
    }
}
