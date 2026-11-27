//! Connexion rapide des services, sans chercher quelles permissions cocher :
//! - jeton d'API : lien de création pré-rempli, collage, vérification en direct auprès de l'API
//!   officielle (et Account ID Cloudflare retrouvé tout seul) ;
//! - GitHub : flux par code d'appareil (app GitHub déclarée par l'éditeur, aucun secret client) ;
//! - OAuth : scopes recommandés par service (sans eux, Cloudflare n'accorde que la lecture) ;
//! - profils de navigateur : la page d'un client s'ouvre dans la session de son compte.
//!
//! Aucune valeur de jeton n'est journalisée ni reprise dans un message d'erreur.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

// ─── Jetons d'API ────────────────────────────────────────────

/// Service dont un jeton d'API se crée par un lien pré-rempli puis se colle dans McpHub.
#[derive(Debug, PartialEq)]
pub struct KeyConnector {
    pub id: &'static str,
    pub label: &'static str,
    /// Variables qu'un même jeton remplit (le jeton Cloudflare sert à cf-orchestrator et au MCP).
    pub vars: &'static [&'static str],
    /// Consignes affichées à côté du lien.
    pub steps: &'static str,
}

/// Variable remplie automatiquement après la vérification d'un jeton Cloudflare.
pub const CLOUDFLARE_ACCOUNT_VAR: &str = "CLOUDFLARE_ACCOUNT_ID";

pub const KEY_CONNECTORS: &[KeyConnector] = &[
    KeyConnector {
        id: "cloudflare",
        label: "Cloudflare",
        vars: &["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_MCP_TOKEN"],
        steps: "The form is pre-filled with the permissions McpHub needs. Keep Zone resources on All zones, pick an expiry, then Continue to summary and Create Token. If Cloudflare lists unrecognised permissions, add them by hand with Edit.",
    },
    KeyConnector {
        id: "supabase",
        label: "Supabase",
        vars: &["SUPABASE_ACCESS_TOKEN"],
        steps: "Click Generate new token, name it McpHub, then copy the token.",
    },
    KeyConnector {
        id: "github",
        label: "GitHub",
        vars: &["GITHUB_PERSONAL_ACCESS_TOKEN", "GITHUB_TOKEN"],
        steps: "The form is pre-filled (name, one year, permissions). Choose Repository access, then Generate token. For an organisation, pick it by hand as Resource owner: GitHub ignores it in the link.",
    },
];

/// Permissions du modèle de jeton de compte Cloudflare (déploiement Workers, Pages, D1, KV, R2,
/// DNS). `account_settings` en lecture permet de retrouver l'Account ID.
const CLOUDFLARE_PERMISSIONS: &[(&str, &str)] = &[
    ("account_settings", "read"),
    ("page", "edit"),
    ("workers_scripts", "edit"),
    ("workers_kv_storage", "edit"),
    ("workers_routes", "edit"),
    ("workers_r2", "edit"),
    ("d1", "edit"),
    ("queues", "edit"),
    ("containers", "edit"),
    ("workers_ai", "edit"),
    ("vectorize", "edit"),
    ("hyperdrive", "edit"),
    ("workers_observability", "edit"),
    ("turnstile", "edit"),
    ("email_routing_addresses", "edit"),
    ("dns", "edit"),
    ("zone", "edit"),
    ("zone_settings", "edit"),
    ("ssl_and_certificates", "edit"),
    ("firewall_services", "edit"),
    ("page_rules", "edit"),
    ("workers_tail", "read"),
    ("cache", "purge"),
];

/// Permissions du jeton GitHub fine-grained (dépôts, Actions, Pages, secrets).
const GITHUB_PERMISSIONS: &[(&str, &str)] = &[
    ("administration", "write"),
    ("contents", "write"),
    ("workflows", "write"),
    ("actions", "write"),
    ("secrets", "write"),
    ("actions_variables", "write"),
    ("environments", "write"),
    ("pages", "write"),
    ("pull_requests", "write"),
    ("issues", "write"),
    ("repository_hooks", "write"),
    ("deployments", "write"),
    ("statuses", "write"),
];

pub fn for_var(var: &str) -> Option<&'static KeyConnector> {
    KEY_CONNECTORS.iter().find(|c| c.vars.contains(&var))
}

/// Encodage d'URL strict : tout sauf les caractères non réservés (RFC 3986).
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Nom donné au jeton chez le service : `McpHub`, ou `McpHub (acme)` pour un compte client.
pub fn token_name(tenant: Option<&str>) -> String {
    match tenant {
        Some(t) => format!("McpHub ({})", t),
        None => "McpHub".into(),
    }
}

/// Page de création du jeton, pré-remplie quand le service le permet.
pub fn create_url(connector: &KeyConnector, name: &str) -> String {
    match connector.id {
        "cloudflare" => {
            let keys: Vec<Value> = CLOUDFLARE_PERMISSIONS
                .iter()
                .map(|(key, kind)| json!({ "key": key, "type": kind }))
                .collect();
            format!(
                "https://dash.cloudflare.com/?to=/:account/api-tokens&permissionGroupKeys={}&name={}",
                pct(&Value::Array(keys).to_string()),
                pct(name)
            )
        }
        "github" => {
            let perms: String = GITHUB_PERMISSIONS
                .iter()
                .map(|(p, level)| format!("&{}={}", p, level))
                .collect();
            format!(
                "https://github.com/settings/personal-access-tokens/new?name={}&description={}&expires_in=366{}",
                pct(name),
                pct("Access used by McpHub agents"),
                perms
            )
        }
        _ => "https://supabase.com/dashboard/account/tokens".into(),
    }
}

/// Contrôle de forme avant tout appel réseau (jeton tronqué, mauvais champ copié).
pub fn check_format(connector: &KeyConnector, value: &str) -> Result<(), String> {
    if value.len() < 20 || value.len() > 512 {
        return Err(format!(
            "this does not look like a {} token (length)",
            connector.label
        ));
    }
    if !value.chars().all(|c| c.is_ascii_graphic()) {
        return Err(format!(
            "this does not look like a {} token (spaces or special characters)",
            connector.label
        ));
    }
    let prefixes: &[&str] = match connector.id {
        "github" => &["github_pat_", "ghp_", "gho_", "ghu_"],
        "supabase" => &["sbp_"],
        _ => &[],
    };
    if !prefixes.is_empty() && !prefixes.iter().any(|p| value.starts_with(p)) {
        return Err(format!(
            "this is not a {} token (expected {}...)",
            connector.label, prefixes[0]
        ));
    }
    Ok(())
}

/// Résultat d'une vérification réussie (aucune valeur secrète).
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    /// Account ID Cloudflare, quand le jeton n'en voit qu'un.
    pub account_id: Option<String>,
    /// Ce que le jeton voit, pour le message de confirmation.
    pub detail: String,
    /// Échéance donnée par le service (millisecondes), quand il la fournit.
    pub expires_at: Option<u64>,
}

/// Échec de vérification : jeton refusé (rien n'est enregistré) ou service injoignable.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyError {
    Refused(String),
    Unreachable(String),
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("McpHub/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

/// Réponse d'une API de service : statut, corps JSON, échéance annoncée par GitHub.
struct ApiReply {
    status: u16,
    body: Value,
    github_expiration: Option<String>,
}

/// Lecture d'une réponse JSON de l'API officielle du service (adresse fixe, jamais fournie
/// par l'appelant).
async fn api_get(
    connector: &KeyConnector,
    url: &str,
    token: &str,
) -> Result<ApiReply, VerifyError> {
    let r = http()
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            VerifyError::Unreachable(format!(
                "{} unreachable: {}",
                connector.label,
                e.without_url()
            ))
        })?;
    let status = r.status().as_u16();
    let github_expiration = r
        .headers()
        .get("github-authentication-token-expiration")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = r.json::<Value>().await.unwrap_or(Value::Null);
    Ok(ApiReply {
        status,
        body,
        github_expiration,
    })
}

/// Interprète la liste des comptes Cloudflare vus par le jeton.
fn cloudflare_accounts(status: u16, body: &Value) -> Result<Verified, VerifyError> {
    let ok = status == 200 && body.get("success").and_then(|s| s.as_bool()) == Some(true);
    if !ok {
        return Err(VerifyError::Refused("Cloudflare refused this token".into()));
    }
    let accounts: Vec<(String, String)> = body
        .get("result")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|acc| {
                    let id = acc.get("id")?.as_str()?.to_string();
                    let name = acc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some((id, name))
                })
                .collect()
        })
        .unwrap_or_default();
    match accounts.as_slice() {
        [] => Err(VerifyError::Refused(
            "the token is valid but sees no account: add the Account Settings Read permission"
                .into(),
        )),
        [(id, name)] => Ok(Verified {
            account_id: Some(id.clone()),
            detail: format!("Cloudflare account {}", name),
            expires_at: None,
        }),
        many => Ok(Verified {
            account_id: None,
            detail: format!(
                "{} Cloudflare accounts: set {} by hand",
                many.len(),
                CLOUDFLARE_ACCOUNT_VAR
            ),
            expires_at: None,
        }),
    }
}

/// Échéance d'un jeton Cloudflare (`/tokens/verify`) ; un jeton inactif est refusé.
fn cloudflare_expiry(body: &Value) -> Result<Option<u64>, VerifyError> {
    let result = body.get("result");
    match result
        .and_then(|r| r.get("status"))
        .and_then(|s| s.as_str())
    {
        Some("active") | None => {}
        Some(other) => {
            return Err(VerifyError::Refused(format!(
                "Cloudflare reports this token as {}",
                other
            )))
        }
    }
    Ok(result
        .and_then(|r| r.get("expires_on"))
        .and_then(|e| e.as_str())
        .and_then(crate::token_watch::parse_datetime))
}

/// Vérifie le jeton auprès de l'API officielle du service.
pub async fn verify(connector: &KeyConnector, token: &str) -> Result<Verified, VerifyError> {
    let hide = |e: VerifyError| match e {
        VerifyError::Refused(m) => VerifyError::Refused(m.replace(token, "***")),
        VerifyError::Unreachable(m) => VerifyError::Unreachable(m.replace(token, "***")),
    };
    let out = match connector.id {
        "cloudflare" => {
            let r = api_get(
                connector,
                "https://api.cloudflare.com/client/v4/accounts?per_page=50",
                token,
            )
            .await?;
            let mut verified = cloudflare_accounts(r.status, &r.body)?;
            // Échéance : route de vérification du jeton de compte, sinon celle du jeton d'utilisateur.
            let mut urls = Vec::new();
            if let Some(id) = &verified.account_id {
                urls.push(format!(
                    "https://api.cloudflare.com/client/v4/accounts/{}/tokens/verify",
                    id
                ));
            }
            urls.push("https://api.cloudflare.com/client/v4/user/tokens/verify".to_string());
            for url in urls {
                // Le jeton est déjà reconnu : sans réponse ici, l'échéance reste simplement inconnue.
                let Ok(r) = api_get(connector, &url, token).await else {
                    break;
                };
                if r.status == 200 {
                    verified.expires_at = cloudflare_expiry(&r.body)?;
                    break;
                }
            }
            Ok(verified)
        }
        "github" => {
            let r = api_get(connector, "https://api.github.com/user", token).await?;
            match r.body.get("login").and_then(|l| l.as_str()) {
                Some(login) if r.status == 200 => Ok(Verified {
                    account_id: None,
                    detail: format!("GitHub account {}", login),
                    expires_at: r
                        .github_expiration
                        .as_deref()
                        .and_then(crate::token_watch::parse_datetime),
                }),
                _ => Err(VerifyError::Refused("GitHub refused this token".into())),
            }
        }
        _ => {
            let r = api_get(connector, "https://api.supabase.com/v1/projects", token).await?;
            match r.body.as_array() {
                Some(projects) if r.status == 200 => Ok(Verified {
                    account_id: None,
                    detail: format!("{} Supabase project(s)", projects.len()),
                    expires_at: None,
                }),
                _ => Err(VerifyError::Refused("Supabase refused this token".into())),
            }
        }
    };
    out.map_err(hide)
}

/// Ce qu'un coffre doit recevoir : le nom visé, plus les emplacements vides que le même jeton
/// (ou l'Account ID retrouvé) peut remplir. `slots` : variable → noms dans le coffre.
#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    Token,
    AccountId(String),
}

pub fn plan_writes(
    name: &str,
    connector: Option<&KeyConnector>,
    account_id: Option<&str>,
    slots: &BTreeMap<String, BTreeSet<String>>,
    present: &BTreeSet<String>,
) -> Vec<(String, Fill)> {
    let mut writes = vec![(name.to_string(), Fill::Token)];
    let Some(c) = connector else {
        return writes;
    };
    let mut push = |slot: &String, fill: Fill| {
        if !present.contains(slot) && !writes.iter().any(|(n, _)| n == slot) {
            writes.push((slot.clone(), fill));
        }
    };
    for var in c.vars {
        for slot in slots.get(*var).into_iter().flatten() {
            push(slot, Fill::Token);
        }
    }
    if let Some(id) = account_id {
        for slot in slots.get(CLOUDFLARE_ACCOUNT_VAR).into_iter().flatten() {
            push(slot, Fill::AccountId(id.to_string()));
        }
    }
    writes
}

/// Description d'un connecteur pour le tableau de bord (adresses et consignes, aucun secret).
pub fn describe(var: &str, tenant: Option<&str>) -> Value {
    match for_var(var) {
        None => Value::Null,
        Some(c) => json!({
            "id": c.id,
            "label": c.label,
            "create_url": create_url(c, &token_name(tenant)),
            "steps": c.steps,
            "device": c.id == "github" && github_client_id().is_some(),
        }),
    }
}

// ─── OAuth : scopes recommandés ──────────────────────────────

/// Code Mode Cloudflare : écriture sur ce qu'un agent déploie (Workers, Pages, D1, KV, R2,
/// DNS), plus `offline_access` pour le renouvellement. Sans liste, le serveur n'annonce que
/// `user:read account:read` et la connexion reste en lecture seule.
const CLOUDFLARE_OAUTH_SCOPES: &str =
    "offline_access user:read account:read account-settings.read \
workers-scripts.write workers-kv-storage.write workers-r2.write workers-routes.write d1.write \
page.write queues.write vectorize.write ai.write containers.write workers-observability.read \
workers-tail.read dns.write zone.read zone-settings.write ssl-and-certificates.write cache.purge";

const SUPABASE_OAUTH_SCOPES: &str =
    "organizations:read projects:read projects:write database:read \
database:write analytics:read secrets:read edge_functions:read edge_functions:write \
environment:read environment:write storage:read storage:write";

/// Scopes à demander pour un serveur MCP distant connu, sinon ceux qu'il annonce.
pub fn oauth_scope(url: &str) -> Option<&'static str> {
    let host = reqwest::Url::parse(url)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    match host.as_str() {
        "mcp.cloudflare.com" => Some(CLOUDFLARE_OAUTH_SCOPES),
        "mcp.supabase.com" => Some(SUPABASE_OAUTH_SCOPES),
        _ => None,
    }
}

// ─── GitHub : code d'appareil ────────────────────────────────

fn valid_client_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Identifiant public de l'app GitHub de l'éditeur : fixé à la compilation
/// (`MCPHUB_GITHUB_CLIENT_ID`) ou `settings.githubClientId` dans config.json. Ce n'est pas un
/// secret : le flux par code d'appareil n'en utilise aucun.
pub fn github_client_id() -> Option<String> {
    if let Some(id) = option_env!("MCPHUB_GITHUB_CLIENT_ID").filter(|id| valid_client_id(id)) {
        return Some(id.to_string());
    }
    let path = crate::config::dedicated_config_path();
    let raw = crate::config::read_raw(&path).ok().flatten()?;
    raw.get("settings")
        .and_then(|s| s.get("githubClientId"))
        .and_then(|v| v.as_str())
        .filter(|id| valid_client_id(id))
        .map(String::from)
}

/// Code à saisir sur github.com/login/device.
pub struct DeviceStart {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    device_code: String,
    interval: u64,
    client_id: String,
}

const GITHUB_DEVICE_SCOPE: &str = "repo read:org workflow";

pub async fn github_device_start(client_id: &str) -> Result<DeviceStart, String> {
    let r = http()
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", GITHUB_DEVICE_SCOPE)])
        .send()
        .await
        .map_err(|e| format!("GitHub unreachable: {}", e.without_url()))?;
    let v: Value = r
        .json()
        .await
        .map_err(|e| format!("GitHub: unexpected answer ({})", e))?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    match (
        get("device_code"),
        get("user_code"),
        get("verification_uri"),
    ) {
        (Some(device_code), Some(user_code), Some(uri))
            if uri.starts_with("https://github.com/") =>
        {
            Ok(DeviceStart {
                user_code,
                verification_uri: uri,
                expires_in: v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(900),
                device_code,
                interval: v
                    .get("interval")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(5)
                    .max(1),
                client_id: client_id.to_string(),
            })
        }
        _ => Err(format!(
            "GitHub refused the device sign-in: {}",
            get("error_description")
                .or_else(|| get("error"))
                .unwrap_or_else(|| "no code returned".into())
        )),
    }
}

/// Résultat d'une interrogation du serveur de jetons pendant le flux par code d'appareil.
#[derive(Debug, PartialEq)]
enum DevicePoll {
    Token(String),
    Wait { slow_down: bool },
    Failed(String),
}

fn parse_device_poll(v: &Value) -> DevicePoll {
    if let Some(token) = v.get("access_token").and_then(|t| t.as_str()) {
        return DevicePoll::Token(token.to_string());
    }
    match v.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending") => DevicePoll::Wait { slow_down: false },
        Some("slow_down") => DevicePoll::Wait { slow_down: true },
        Some("expired_token") => DevicePoll::Failed("the code expired: start again".into()),
        Some("access_denied") => DevicePoll::Failed("sign-in cancelled on GitHub".into()),
        Some(other) => DevicePoll::Failed(format!("GitHub refused the sign-in ({})", other)),
        None => DevicePoll::Failed("GitHub: unexpected answer".into()),
    }
}

/// Attend l'autorisation sur GitHub et renvoie le jeton.
pub async fn github_device_wait(start: DeviceStart) -> Result<String, String> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(start.expires_in.min(1800));
    let mut interval = start.interval;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if std::time::Instant::now() >= deadline {
            return Err("the code expired: start again".into());
        }
        let r = http()
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", start.client_id.as_str()),
                ("device_code", start.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await;
        let Ok(r) = r else {
            continue;
        };
        let v: Value = r.json().await.unwrap_or(Value::Null);
        match parse_device_poll(&v) {
            DevicePoll::Token(t) => return Ok(t),
            DevicePoll::Wait { slow_down } => {
                if slow_down {
                    interval += 5;
                }
            }
            DevicePoll::Failed(e) => return Err(e),
        }
    }
}

/// État d'une connexion par code d'appareil, lu par le tableau de bord.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    Pending,
    Done(String),
    Failed(String),
}

fn device_states() -> std::sync::MutexGuard<'static, HashMap<String, DeviceState>> {
    static S: OnceLock<Mutex<HashMap<String, DeviceState>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn device_set(id: &str, state: DeviceState) {
    let mut states = device_states();
    // Borne la table : les connexions anciennes terminées n'intéressent plus personne.
    if states.len() > 64 {
        states.retain(|_, s| *s == DeviceState::Pending);
    }
    states.insert(id.to_string(), state);
}

pub fn device_get(id: &str) -> Option<DeviceState> {
    device_states().get(id).cloned()
}

// ─── Profils de navigateur ───────────────────────────────────

/// Navigateurs Chromium dont les profils se listent (dossier sous Application Support).
const BROWSERS: &[(&str, &str)] = &[
    ("Google Chrome", "Google/Chrome"),
    ("Brave Browser", "BraveSoftware/Brave-Browser"),
    ("Microsoft Edge", "Microsoft Edge"),
    ("Chromium", "Chromium"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct BrowserProfile {
    pub app: String,
    pub dir: String,
    pub name: String,
}

/// Profils lus dans le fichier `Local State` d'un navigateur.
fn parse_local_state(app: &str, raw: &str) -> Vec<BrowserProfile> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(cache) = v
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
    else {
        return Vec::new();
    };
    let mut out: Vec<BrowserProfile> = cache
        .iter()
        .map(|(dir, info)| {
            let label = info.get("name").and_then(|n| n.as_str()).unwrap_or(dir);
            let account = info
                .get("user_name")
                .and_then(|n| n.as_str())
                .filter(|u| !u.is_empty());
            BrowserProfile {
                app: app.to_string(),
                dir: dir.clone(),
                name: match account {
                    Some(a) => format!("{} ({})", label, a),
                    None => label.to_string(),
                },
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn browser_profiles() -> Vec<BrowserProfile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    BROWSERS
        .iter()
        .flat_map(|(app, dir)| {
            let path = home
                .join("Library/Application Support")
                .join(dir)
                .join("Local State");
            std::fs::read_to_string(path)
                .map(|raw| parse_local_state(app, &raw))
                .unwrap_or_default()
        })
        .collect()
}

/// Adresse ouvrable : HTTPS, sans espace ni caractère de contrôle.
pub fn openable(url: &str) -> bool {
    url.starts_with("https://") && url.len() < 8192 && url.chars().all(|c| c.is_ascii_graphic())
}

/// Ouvre une adresse dans le navigateur par défaut, ou dans un profil précis (déjà détecté).
pub fn open_url(url: &str, profile: Option<&BrowserProfile>) -> Result<(), String> {
    if !openable(url) {
        return Err("only https addresses can be opened".into());
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("/usr/bin/open");
        match profile {
            Some(p) => {
                cmd.args(["-na", &p.app, "--args"])
                    .arg(format!("--profile-directory={}", p.dir))
                    .arg(url);
            }
            None => {
                cmd.arg(url);
            }
        }
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot open the browser: {}", e))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        Err("opening a browser is only supported on macOS: copy the address".into())
    }
}

/// Profil de navigateur choisi pour un tenant (`browser` de son projet dans projects.json),
/// seulement s'il existe encore sur ce Mac.
pub fn tenant_browser(projects: &Value, tenant: &str) -> Option<BrowserProfile> {
    let pref = projects
        .get("projects")?
        .as_object()?
        .values()
        .find(|p| p.get("tenant").and_then(|t| t.as_str()) == Some(tenant))?
        .get("browser")?;
    let app = pref.get("app")?.as_str()?;
    let dir = pref.get("profile")?.as_str()?;
    browser_profiles()
        .into_iter()
        .find(|p| p.app == app && p.dir == dir)
}

/// Enregistre (ou retire avec `None`) le profil de navigateur d'un tenant dans projects.json.
pub fn set_tenant_browser(
    json: &mut Value,
    tenant: &str,
    profile: Option<&BrowserProfile>,
) -> Result<(), String> {
    let entry = json
        .get_mut("projects")
        .and_then(|p| p.as_object_mut())
        .and_then(|p| {
            p.values_mut()
                .find(|p| p.get("tenant").and_then(|t| t.as_str()) == Some(tenant))
        })
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| format!("tenant {} inconnu", tenant))?;
    match profile {
        Some(p) => {
            entry.insert("browser".into(), json!({ "app": p.app, "profile": p.dir }));
        }
        None => {
            entry.remove("browser");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connecteur_par_variable() {
        assert_eq!(
            for_var("CLOUDFLARE_MCP_TOKEN").map(|c| c.id),
            Some("cloudflare")
        );
        assert_eq!(
            for_var("GITHUB_PERSONAL_ACCESS_TOKEN").map(|c| c.id),
            Some("github")
        );
        assert_eq!(
            for_var("SUPABASE_ACCESS_TOKEN").map(|c| c.id),
            Some("supabase")
        );
        assert!(for_var("CLOUDFLARE_ACCOUNT_ID").is_none());
    }

    #[test]
    fn lien_cloudflare_pre_rempli() {
        let url = create_url(
            for_var("CLOUDFLARE_API_TOKEN").unwrap(),
            &token_name(Some("acme")),
        );
        assert!(url.starts_with(
            "https://dash.cloudflare.com/?to=/:account/api-tokens&permissionGroupKeys=%5B%7B"
        ));
        assert!(url.contains("%22workers_scripts%22%2C%22type%22%3A%22edit%22"));
        assert!(url.ends_with("&name=McpHub%20%28acme%29"));
    }

    #[test]
    fn lien_github_pre_rempli() {
        let url = create_url(for_var("GITHUB_TOKEN").unwrap(), "McpHub");
        assert!(
            url.starts_with("https://github.com/settings/personal-access-tokens/new?name=McpHub&")
        );
        assert!(url.contains("&expires_in=366&administration=write&contents=write"));
    }

    #[test]
    fn forme_du_jeton() {
        let gh = for_var("GITHUB_TOKEN").unwrap();
        assert!(check_format(gh, "github_pat_11ABCDEFGHIJKLMNOPQRSTUV").is_ok());
        assert!(check_format(gh, "sbp_0123456789abcdef0123456789").is_err());
        assert!(check_format(gh, "github_pat_ espace dans le jeton").is_err());
        let cf = for_var("CLOUDFLARE_API_TOKEN").unwrap();
        assert!(check_format(cf, "court").is_err());
        assert!(check_format(cf, "AbCdEf0123456789_AbCdEf0123456789-xyz").is_ok());
    }

    #[test]
    fn comptes_cloudflare() {
        let one = json!({"success": true, "result": [{"id": "abc123", "name": "Acme"}]});
        assert_eq!(
            cloudflare_accounts(200, &one)
                .unwrap()
                .account_id
                .as_deref(),
            Some("abc123")
        );
        let two = json!({"success": true, "result": [{"id": "a", "name": "A"}, {"id": "b", "name": "B"}]});
        assert_eq!(cloudflare_accounts(200, &two).unwrap().account_id, None);
        let none = json!({"success": true, "result": []});
        assert!(matches!(
            cloudflare_accounts(200, &none),
            Err(VerifyError::Refused(_))
        ));
        let refused = json!({"success": false, "errors": [{"code": 1000}]});
        assert!(matches!(
            cloudflare_accounts(403, &refused),
            Err(VerifyError::Refused(_))
        ));
    }

    #[test]
    fn echeance_cloudflare() {
        let active = json!({"success": true, "result": {"status": "active", "expires_on": "2027-01-01T00:00:00Z"}});
        assert_eq!(
            cloudflare_expiry(&active),
            Ok(crate::token_watch::parse_datetime("2027-01-01T00:00:00Z"))
        );
        let forever = json!({"success": true, "result": {"status": "active"}});
        assert_eq!(cloudflare_expiry(&forever), Ok(None));
        let expired = json!({"success": true, "result": {"status": "expired"}});
        assert!(matches!(
            cloudflare_expiry(&expired),
            Err(VerifyError::Refused(_))
        ));
    }

    #[test]
    fn ecritures_prevues() {
        let cf = for_var("CLOUDFLARE_API_TOKEN");
        let mut slots: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        slots.insert(
            "CLOUDFLARE_API_TOKEN".into(),
            ["CLOUDFLARE_API_TOKEN".to_string()].into(),
        );
        slots.insert(
            "CLOUDFLARE_MCP_TOKEN".into(),
            ["CLOUDFLARE_MCP_TOKEN".to_string()].into(),
        );
        slots.insert(
            CLOUDFLARE_ACCOUNT_VAR.into(),
            ["CLOUDFLARE_ACCOUNT_ID".to_string()].into(),
        );
        let present: BTreeSet<String> = ["CLOUDFLARE_MCP_TOKEN".to_string()].into();
        let writes = plan_writes("CLOUDFLARE_API_TOKEN", cf, Some("abc"), &slots, &present);
        assert_eq!(
            writes,
            vec![
                ("CLOUDFLARE_API_TOKEN".to_string(), Fill::Token),
                (
                    "CLOUDFLARE_ACCOUNT_ID".to_string(),
                    Fill::AccountId("abc".into())
                ),
            ]
        );
        // Un secret inconnu des connecteurs ne remplit que lui-même.
        assert_eq!(
            plan_writes("AUTRE", None, None, &slots, &present),
            vec![("AUTRE".to_string(), Fill::Token)]
        );
    }

    #[test]
    fn scopes_oauth_par_service() {
        assert!(oauth_scope("https://mcp.cloudflare.com/mcp")
            .unwrap()
            .contains("workers-scripts.write"));
        assert!(oauth_scope("https://mcp.cloudflare.com/mcp")
            .unwrap()
            .starts_with("offline_access"));
        // Nom refusé par Cloudflare (« Unknown OAuth scope ») : le bon est page.write.
        assert!(!oauth_scope("https://mcp.cloudflare.com/mcp")
            .unwrap()
            .contains("pages.write"));
        assert!(oauth_scope("https://mcp.supabase.com/mcp?project_ref=x")
            .unwrap()
            .contains("database:write"));
        assert!(oauth_scope("https://mcp.stripe.com").is_none());
    }

    #[test]
    fn reponses_du_code_d_appareil() {
        assert_eq!(
            parse_device_poll(&json!({"access_token": "ghu_x"})),
            DevicePoll::Token("ghu_x".into())
        );
        assert_eq!(
            parse_device_poll(&json!({"error": "authorization_pending"})),
            DevicePoll::Wait { slow_down: false }
        );
        assert_eq!(
            parse_device_poll(&json!({"error": "slow_down"})),
            DevicePoll::Wait { slow_down: true }
        );
        assert!(matches!(
            parse_device_poll(&json!({"error": "access_denied"})),
            DevicePoll::Failed(_)
        ));
        assert!(valid_client_id("Iv23liAbCdEf0123"));
        assert!(!valid_client_id("id avec espace"));
    }

    #[test]
    fn profils_de_navigateur() {
        let raw = r#"{"profile":{"info_cache":{"Default":{"name":"Perso"},"Profile 2":{"name":"Client","user_name":"x@exemple.fr"}}}}"#;
        let profiles = parse_local_state("Google Chrome", raw);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "Client (x@exemple.fr)");
        assert_eq!(profiles[0].dir, "Profile 2");
        assert!(parse_local_state("Google Chrome", "pas du json").is_empty());
    }

    #[test]
    fn profil_enregistre_dans_projects() {
        let mut json =
            json!({"projects": {"acme-site": {"tenant": "acme", "workspaces": ["/tmp/a"]}}});
        let p = BrowserProfile {
            app: "Google Chrome".into(),
            dir: "Profile 2".into(),
            name: "Client".into(),
        };
        set_tenant_browser(&mut json, "acme", Some(&p)).unwrap();
        assert_eq!(
            json["projects"]["acme-site"]["browser"]["profile"],
            "Profile 2"
        );
        set_tenant_browser(&mut json, "acme", None).unwrap();
        assert!(json["projects"]["acme-site"].get("browser").is_none());
        assert!(set_tenant_browser(&mut json, "inconnu", None).is_err());
    }

    #[test]
    fn adresses_ouvrables() {
        assert!(openable("https://github.com/login/device"));
        assert!(!openable("http://exemple.fr"));
        assert!(!openable("https://exemple.fr/a b"));
        assert!(!openable("file:///etc/passwd"));
    }
}
