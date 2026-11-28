//! Serveur d'autorisation OAuth 2.1 de l'accès distant.
//!
//! - Découverte : métadonnées de la ressource protégée (RFC 9728) et du serveur (RFC 8414).
//! - Inscription des clients : dynamique (RFC 7591) ou par document de métadonnées publié à
//!   l'adresse qui sert d'identifiant (CIMD, recommandé par la spécification MCP).
//! - Autorisation : code + PKCE S256 obligatoire. Chaque connexion d'un client est validée par
//!   une personne sur la machine de McpHub (fenêtre macOS, ou code d'appairage à taper dans
//!   `McpHub remote approve` quand il n'y a pas d'écran). S'inscrire ne donne donc aucun accès.
//! - Jetons : accès d'une heure gardé en mémoire, rafraîchissement de 30 jours renouvelé à
//!   chaque usage. Seules leurs empreintes SHA-256 sont conservées, dans
//!   `~/.McpHub/remote/clients.json` (mode 600, caché des serveurs isolés).
//! - Révocation : supprimer un client (CLI ou tableau de bord) coupe aussitôt ses jetons, même
//!   ceux déjà en mémoire, parce que chaque appel vérifie que le client existe encore.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::http::{html_escape, urlencode, Request, Response};

const ACCESS_TTL: u64 = 3600;
const REFRESH_TTL: u64 = 30 * 24 * 3600;
const CODE_TTL: u64 = 120;
const PAIRING_TTL: u64 = 600;
/// Sous le délai d'environ 100 s après lequel Cloudflare coupe une réponse qui tarde.
const CONSENT_TIMEOUT: u64 = 90;
/// Client inscrit mais jamais autorisé : oublié au bout de 24 h.
const UNAPPROVED_TTL: u64 = 24 * 3600;
const MAX_CLIENTS: usize = 100;
const MAX_PENDING: usize = 20;
const MAX_REDIRECTS: usize = 10;
const CIMD_MAX_BYTES: usize = 64 * 1024;
const CIMD_REFRESH: u64 = 3600;

// ─── Stockage ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RefreshGrant {
    pub hash: String,
    pub expires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Client {
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// "dcr" (inscription dynamique) ou "cimd" (document de métadonnées).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_hash: Option<String>,
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    #[serde(default)]
    pub refresh: Vec<RefreshGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Store {
    #[serde(default)]
    pub clients: BTreeMap<String, Client>,
}

pub fn store_path() -> PathBuf {
    super::remote_dir().join("clients.json")
}

pub fn load_store() -> Result<Store, String> {
    let path = store_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("{} illisible : {}", path.display(), e))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("{} illisible : {}", path.display(), e)),
    }
}

pub fn save_store(store: &Store) -> Result<(), String> {
    let path = store_path();
    super::ensure_private_dir()?;
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("{} : {}", tmp.display(), e))?;
    crate::vault::restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| format!("{} : {}", path.display(), e))
}

fn store_mtime() -> Option<SystemTime> {
    std::fs::metadata(store_path())
        .and_then(|m| m.modified())
        .ok()
}

// ─── État en mémoire ─────────────────────────────────────────

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    expires: u64,
}

struct Access {
    client_id: String,
    expires: u64,
}

struct Pending {
    code: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    challenge: String,
    created: u64,
    approved: bool,
}

#[derive(Default)]
struct State {
    store: Store,
    loaded: bool,
    mtime: Option<SystemTime>,
    codes: HashMap<String, Code>,
    access: HashMap<String, Access>,
    pending: HashMap<String, Pending>,
    hits: HashMap<String, VecDeque<Instant>>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

impl State {
    /// Relit le fichier si la CLI l'a changé (révocation, nouveau client).
    fn sync(&mut self) -> Result<(), String> {
        let mtime = store_mtime();
        if !self.loaded || mtime != self.mtime {
            self.store = load_store()?;
            self.loaded = true;
            self.mtime = mtime;
            let clients = &self.store.clients;
            self.access
                .retain(|_, a| clients.contains_key(&a.client_id));
        }
        Ok(())
    }

    fn save(&mut self) -> Result<(), String> {
        save_store(&self.store)?;
        self.mtime = store_mtime();
        Ok(())
    }

    fn prune(&mut self, now: u64) {
        self.codes.retain(|_, c| c.expires > now);
        self.access.retain(|_, a| a.expires > now);
        self.pending.retain(|_, p| p.created + PAIRING_TTL > now);
        self.store.clients.retain(|_, c| {
            c.approved_at.is_some() || c.kind == "cimd" || c.created + UNAPPROVED_TTL > now
        });
        for client in self.store.clients.values_mut() {
            client.refresh.retain(|r| r.expires > now);
        }
    }

    /// Fenêtre glissante : au plus `max` passages en `window` secondes pour cette clé.
    fn allow(&mut self, key: String, max: usize, window: u64) -> bool {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(window);
        let hits = self.hits.entry(key).or_default();
        while hits
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            hits.pop_front();
        }
        if hits.len() >= max {
            return false;
        }
        hits.push_back(now);
        if self.hits.len() > 10_000 {
            self.hits.retain(|_, v| !v.is_empty());
        }
        true
    }
}

/// Limite de débit partagée avec le module principal (appels MCP).
pub fn rate_allow(key: String, max: usize, window: u64) -> bool {
    state().allow(key, max, window)
}

// ─── Outils ──────────────────────────────────────────────────

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("source aleatoire indisponible : {}", e))?;
    Ok(buf)
}

fn random_token(prefix: &str) -> Result<String, String> {
    Ok(format!(
        "{}{}",
        prefix,
        crate::oauth::b64url(&random_bytes(32)?)
    ))
}

pub fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn same(a: &str, b: &str) -> bool {
    crate::http_guard::token_matches(a, b)
}

/// Code d'appairage lisible : 8 caractères sans 0/O ni 1/I, en deux groupes.
fn pairing_code() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let raw = random_bytes(8)?;
    let chars: String = raw
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    Ok(format!("{}-{}", &chars[..4], &chars[4..]))
}

fn normalize_pairing(code: &str) -> String {
    let clean: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if clean.len() == 8 {
        format!("{}-{}", &clean[..4], &clean[4..])
    } else {
        clean
    }
}

/// Adresse de retour acceptée : https, http sur la boucle locale (applications de bureau),
/// ou schéma privé en notation inverse de domaine (RFC 8252). Jamais de fragment.
pub fn valid_redirect(uri: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(uri) else {
        return false;
    };
    if url.fragment().is_some() || uri.len() > 2048 {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some_and(|h| !h.is_empty()),
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        scheme => {
            scheme.contains('.')
                && !matches!(scheme, "javascript" | "data" | "file" | "vbscript" | "blob")
        }
    }
}

fn redirect_with(redirect_uri: &str, params: &[(&str, &str)]) -> String {
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect();
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    format!("{}{}{}", redirect_uri, sep, query.join("&"))
}

fn client_label(client: &Client) -> String {
    let short: String = client.client_id.chars().take(48).collect();
    if client.name.is_empty() {
        short
    } else {
        format!("{} ({})", client.name, short)
    }
}

// ─── Découverte ──────────────────────────────────────────────

pub fn protected_resource_metadata(base: &str) -> Value {
    json!({
        "resource": format!("{}/mcp", base),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp"],
        "resource_name": "McpHub",
    })
}

pub fn authorization_server_metadata(base: &str) -> Value {
    json!({
        "issuer": base,
        "authorization_endpoint": format!("{}/authorize", base),
        "token_endpoint": format!("{}/token", base),
        "registration_endpoint": format!("{}/register", base),
        "revocation_endpoint": format!("{}/revoke", base),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "revocation_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "scopes_supported": ["mcp"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
    })
}

// ─── Inscription dynamique (RFC 7591) ────────────────────────

pub fn register(req: &Request) -> Response {
    if !rate_allow(format!("register:{}", req.client_ip()), 10, 3600) {
        return Response::oauth_error(429, "slow_down", "too many registrations");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
        return Response::oauth_error(400, "invalid_client_metadata", "body must be JSON");
    };
    let redirects: Vec<String> = body
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if redirects.is_empty() || redirects.len() > MAX_REDIRECTS {
        return Response::oauth_error(
            400,
            "invalid_redirect_uri",
            "1 to 10 redirect_uris are required",
        );
    }
    if let Some(bad) = redirects.iter().find(|u| !valid_redirect(u)) {
        return Response::oauth_error(
            400,
            "invalid_redirect_uri",
            &format!("redirect_uri not allowed: {}", bad),
        );
    }
    let method = body
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    if !matches!(
        method.as_str(),
        "none" | "client_secret_post" | "client_secret_basic"
    ) {
        return Response::oauth_error(
            400,
            "invalid_client_metadata",
            "token_endpoint_auth_method must be none, client_secret_post or client_secret_basic",
        );
    }
    let name: String = body
        .get("client_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();

    let (client_id, secret) = match (random_token("mhc_"), random_token("mhs_")) {
        (Ok(id), Ok(secret)) => (id, secret),
        (Err(e), _) | (_, Err(e)) => return Response::oauth_error(500, "server_error", &e),
    };
    let issued = now();
    let with_secret = method != "none";
    let client = Client {
        client_id: client_id.clone(),
        name: name.clone(),
        redirect_uris: redirects.clone(),
        kind: "dcr".into(),
        secret_hash: with_secret.then(|| digest(&secret)),
        created: issued,
        ..Default::default()
    };

    let mut st = state();
    if let Err(e) = st.sync() {
        return Response::oauth_error(500, "server_error", &e);
    }
    st.prune(issued);
    if st.store.clients.len() >= MAX_CLIENTS {
        return Response::oauth_error(
            400,
            "invalid_client_metadata",
            "too many clients: revoke unused ones with McpHub remote revoke",
        );
    }
    st.store.clients.insert(client_id.clone(), client);
    if let Err(e) = st.save() {
        return Response::oauth_error(500, "server_error", &e);
    }
    drop(st);

    let mut answer = json!({
        "client_id": client_id,
        "client_id_issued_at": issued,
        "client_name": name,
        "redirect_uris": redirects,
        "token_endpoint_auth_method": method,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "scope": "mcp",
    });
    if with_secret {
        answer["client_secret"] = json!(secret);
        answer["client_secret_expires_at"] = json!(0);
    }
    Response::json(201, &answer)
}

// ─── Documents de métadonnées (CIMD) ─────────────────────────

fn forbidden_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] == 0
                || o[0] >= 240
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| forbidden_ip(v4.into()))
        }
    }
}

/// Télécharge le document d'un client identifié par une adresse https. L'hôte doit être
/// public (pas de requête vers le réseau local depuis le démon), la connexion est épinglée à
/// l'adresse vérifiée, sans redirection, 5 s et 64 Ko au plus.
async fn fetch_cimd(client_id: &str) -> Result<Client, String> {
    let url = reqwest::Url::parse(client_id).map_err(|_| "client_id invalide".to_string())?;
    if url.scheme() != "https" || url.fragment().is_some() || url.path() == "/" {
        return Err("client_id doit etre une adresse https avec un chemin".into());
    }
    let host = url
        .host_str()
        .ok_or("client_id sans hote")?
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("hote du client introuvable : {}", e))?
        .collect();
    let first = *addrs.first().ok_or("hote du client sans adresse")?;
    if addrs.iter().any(|a| forbidden_ip(a.ip())) {
        return Err("hote du client sur un reseau prive : refuse".into());
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .resolve(&host, first)
        .user_agent(concat!("McpHub/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("document du client injoignable : {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("document du client : HTTP {}", resp.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > CIMD_MAX_BYTES {
            return Err("document du client trop gros".into());
        }
    }
    let doc: Value =
        serde_json::from_slice(&bytes).map_err(|_| "document du client illisible".to_string())?;
    if doc.get("client_id").and_then(|v| v.as_str()) != Some(client_id) {
        return Err("le document ne porte pas ce client_id".into());
    }
    if doc
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
        .is_some_and(|m| m != "none")
    {
        return Err(
            "seuls les clients publics (token_endpoint_auth_method none) sont acceptes en CIMD"
                .into(),
        );
    }
    let redirects: Vec<String> = doc
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if redirects.is_empty()
        || redirects.len() > MAX_REDIRECTS
        || redirects.iter().any(|u| !valid_redirect(u))
    {
        return Err("redirect_uris absentes ou refusees dans le document du client".into());
    }
    let name: String = doc
        .get("client_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    Ok(Client {
        client_id: client_id.to_string(),
        name,
        redirect_uris: redirects,
        kind: "cimd".into(),
        created: now(),
        fetched_at: Some(now()),
        ..Default::default()
    })
}

/// Client connu, ou document CIMD téléchargé (et mis à jour au bout d'une heure).
async fn resolve_client(client_id: &str) -> Result<Client, String> {
    let known = {
        let mut st = state();
        st.sync()?;
        st.store.clients.get(client_id).cloned()
    };
    let is_url = client_id.starts_with("https://");
    match known {
        Some(c) if !is_url || c.fetched_at.unwrap_or(0) + CIMD_REFRESH > now() => Ok(c),
        Some(old) => {
            let mut fresh = fetch_cimd(client_id).await?;
            fresh.created = old.created;
            fresh.approved_at = old.approved_at;
            fresh.last_used = old.last_used;
            fresh.refresh = old.refresh;
            let mut st = state();
            st.sync()?;
            st.store
                .clients
                .insert(client_id.to_string(), fresh.clone());
            st.save()?;
            Ok(fresh)
        }
        None if is_url => {
            let fresh = fetch_cimd(client_id).await?;
            let mut st = state();
            st.sync()?;
            st.prune(now());
            if st.store.clients.len() >= MAX_CLIENTS {
                return Err("trop de clients : en revoquer avec McpHub remote revoke".into());
            }
            st.store
                .clients
                .insert(client_id.to_string(), fresh.clone());
            st.save()?;
            Ok(fresh)
        }
        None => Err("client inconnu (inscription expiree ou revoquee)".into()),
    }
}

// ─── Autorisation ────────────────────────────────────────────

fn page(req: &Request, title_fr: &str, title_en: &str, body_fr: &str, body_en: &str) -> String {
    let french = req.prefers_french();
    let (title, body) = if french {
        (title_fr, body_fr)
    } else {
        (title_en, body_en)
    };
    format!(
        "<!doctype html><html lang=\"{lang}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>McpHub</title><style>body{{font-family:-apple-system,system-ui,sans-serif;max-width:34rem;margin:12vh auto;padding:0 1rem;color:#1d1d1f;background:#fbfbfd}}h1{{font-size:1.4rem}}code{{background:#f0f0f3;padding:.15rem .4rem;border-radius:6px;font-size:1.05rem}}a.btn{{display:inline-block;margin-top:1rem;padding:.6rem 1.1rem;border-radius:10px;background:#1d1d1f;color:#fff;text-decoration:none}}@media(prefers-color-scheme:dark){{body{{background:#111;color:#f5f5f7}}code{{background:#2a2a2e}}a.btn{{background:#f5f5f7;color:#111}}}}</style></head><body><h1>{title}</h1>{body}</body></html>",
        lang = if french { "fr" } else { "en" },
        title = title,
        body = body,
    )
}

fn error_page(req: &Request, fr: &str, en: &str) -> Response {
    Response::html(
        400,
        page(
            req,
            "Connexion impossible",
            "Cannot connect",
            &format!("<p>{}</p>", html_escape(fr)),
            &format!("<p>{}</p>", html_escape(en)),
        ),
    )
}

fn issue_code(
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state_param: Option<&str>,
    base: &str,
) -> Response {
    let code = match random_token("mha_") {
        Ok(c) => c,
        Err(e) => return Response::oauth_error(500, "server_error", &e),
    };
    let mut st = state();
    st.codes.insert(
        digest(&code),
        Code {
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            challenge: challenge.to_string(),
            expires: now() + CODE_TTL,
        },
    );
    if let Ok(()) = st.sync() {
        if let Some(c) = st.store.clients.get_mut(client_id) {
            c.approved_at = Some(now());
        }
        if let Err(e) = st.save() {
            eprintln!("[McpHub][REMOTE] enregistrement du client : {}", e);
        }
    }
    drop(st);
    let mut params = vec![("code", code.as_str()), ("iss", base)];
    if let Some(s) = state_param {
        params.push(("state", s));
    }
    Response::redirect(&redirect_with(redirect_uri, &params))
}

fn denied(redirect_uri: &str, state_param: Option<&str>, base: &str, why: &str) -> Response {
    let mut params = vec![
        ("error", "access_denied"),
        ("error_description", why),
        ("iss", base),
    ];
    if let Some(s) = state_param {
        params.push(("state", s));
    }
    Response::redirect(&redirect_with(redirect_uri, &params))
}

pub async fn authorize(req: &Request, base: &str) -> Response {
    let p = req.params();
    let client_id = p.get("client_id").cloned().unwrap_or_default();
    let redirect_uri = p.get("redirect_uri").cloned().unwrap_or_default();
    if client_id.is_empty() {
        return error_page(req, "Paramètre client_id manquant.", "Missing client_id.");
    }
    // Chaque demande peut ouvrir une fenêtre sur le Mac : débit limité par adresse.
    if !rate_allow(format!("authorize:{}", req.client_ip()), 10, 900) {
        return error_page(
            req,
            "Trop de tentatives. Réessayez dans 15 minutes.",
            "Too many attempts. Try again in 15 minutes.",
        );
    }
    let client = match resolve_client(&client_id).await {
        Ok(c) => c,
        Err(e) => {
            return error_page(
                req,
                &format!("Client refusé : {}", e),
                &format!("Client rejected: {}", e),
            )
        }
    };
    // Une seule adresse inscrite : elle peut être omise (RFC 6749 §3.1.2.3).
    let redirect_uri = if redirect_uri.is_empty() && client.redirect_uris.len() == 1 {
        client.redirect_uris[0].clone()
    } else {
        redirect_uri
    };
    if !client.redirect_uris.iter().any(|u| u == &redirect_uri) {
        // Jamais de redirection vers une adresse non inscrite.
        return error_page(
            req,
            "Adresse de retour non inscrite pour ce client.",
            "redirect_uri is not registered for this client.",
        );
    }
    let state_param = p.get("state").map(|s| s.as_str());
    if p.get("response_type").map(|s| s.as_str()) != Some("code") {
        let mut params = vec![("error", "unsupported_response_type"), ("iss", base)];
        if let Some(s) = state_param {
            params.push(("state", s));
        }
        return Response::redirect(&redirect_with(&redirect_uri, &params));
    }
    let challenge = p.get("code_challenge").cloned().unwrap_or_default();
    if challenge.len() < 43
        || challenge.len() > 128
        || p.get("code_challenge_method").map(|s| s.as_str()) != Some("S256")
    {
        let mut params = vec![
            ("error", "invalid_request"),
            ("error_description", "PKCE S256 is required"),
            ("iss", base),
        ];
        if let Some(s) = state_param {
            params.push(("state", s));
        }
        return Response::redirect(&redirect_with(&redirect_uri, &params));
    }
    if let Some(resource) = p.get("resource") {
        let resource = resource.trim_end_matches('/');
        if resource != base && resource != format!("{}/mcp", base) {
            let mut params = vec![
                ("error", "invalid_target"),
                ("error_description", "unknown resource"),
                ("iss", base),
            ];
            if let Some(s) = state_param {
                params.push(("state", s));
            }
            return Response::redirect(&redirect_with(&redirect_uri, &params));
        }
    }
    if let Some(problem) = crate::toolguard::remote_profile_problem() {
        return error_page(
            req,
            &format!("Accès distant fermé sur ce McpHub : {}", problem),
            &format!("Remote access is closed on this McpHub: {}", problem),
        );
    }

    let return_host = reqwest::Url::parse(&redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| redirect_uri.clone());
    let servers = crate::toolguard::remote_profile_servers().join(", ");
    let message = format!(
        "Un service veut se connecter a McpHub depuis Internet.\n\nClient : {}\nRetour vers : {}\nServeurs visibles : {}\nAcces : lecture seule (profil remote)\n\nAutoriser cette connexion ?",
        client_label(&client),
        return_host,
        servers
    );
    let answer = crate::confirm_dialog::ask(message, CONSENT_TIMEOUT).await;
    match answer {
        crate::confirm_dialog::DialogAnswer::Allow => issue_code(
            &client.client_id,
            &redirect_uri,
            &challenge,
            state_param,
            base,
        ),
        crate::confirm_dialog::DialogAnswer::Deny => denied(
            &redirect_uri,
            state_param,
            base,
            "refused on the McpHub machine",
        ),
        crate::confirm_dialog::DialogAnswer::Timeout => denied(
            &redirect_uri,
            state_param,
            base,
            "no answer on the McpHub machine",
        ),
        crate::confirm_dialog::DialogAnswer::Unavailable => {
            start_pairing(req, &client, &redirect_uri, state_param, &challenge)
        }
    }
}

/// Machine sans écran (serveur Linux) : un code à valider avec `McpHub remote approve`.
fn start_pairing(
    req: &Request,
    client: &Client,
    redirect_uri: &str,
    state_param: Option<&str>,
    challenge: &str,
) -> Response {
    let (id, code) = match (random_token("mhp_"), pairing_code()) {
        (Ok(id), Ok(code)) => (id, code),
        (Err(e), _) | (_, Err(e)) => return Response::oauth_error(500, "server_error", &e),
    };
    let mut st = state();
    st.prune(now());
    if st.pending.len() >= MAX_PENDING {
        return error_page(
            req,
            "Trop de connexions en attente. Réessayez plus tard.",
            "Too many pending connections. Try again later.",
        );
    }
    st.pending.insert(
        id.clone(),
        Pending {
            code: code.clone(),
            client_id: client.client_id.clone(),
            redirect_uri: redirect_uri.to_string(),
            state: state_param.map(String::from),
            challenge: challenge.to_string(),
            created: now(),
            approved: false,
        },
    );
    drop(st);
    eprintln!(
        "[McpHub][REMOTE] Connexion de {} en attente : McpHub remote approve {}",
        client_label(client),
        code
    );
    pairing_page(req, &id, &code)
}

fn pairing_page(req: &Request, id: &str, code: &str) -> Response {
    let link = format!("/authorize/continue?p={}", urlencode(id));
    let code = html_escape(code);
    Response::html(
        200,
        page(
            req,
            "Validation sur la machine McpHub",
            "Approve on the McpHub machine",
            &format!("<p>Sur la machine où tourne McpHub, lancez :</p><p><code>McpHub remote approve {code}</code></p><p>puis revenez ici. Ce code expire dans 10 minutes.</p><a class=\"btn\" href=\"{link}\">Continuer</a>"),
            &format!("<p>On the machine running McpHub, run:</p><p><code>McpHub remote approve {code}</code></p><p>then come back here. This code expires in 10 minutes.</p><a class=\"btn\" href=\"{link}\">Continue</a>"),
        ),
    )
}

pub fn authorize_continue(req: &Request, base: &str) -> Response {
    let Some(id) = req.param("p") else {
        return error_page(req, "Lien incomplet.", "Incomplete link.");
    };
    let mut st = state();
    st.prune(now());
    let Some(pending) = st.pending.get(&id) else {
        return error_page(
            req,
            "Demande expirée. Relancez la connexion depuis votre application.",
            "Request expired. Start the connection again from your app.",
        );
    };
    if !pending.approved {
        let code = pending.code.clone();
        drop(st);
        return pairing_page(req, &id, &code);
    }
    let Some(pending) = st.pending.remove(&id) else {
        return error_page(req, "Demande expirée.", "Request expired.");
    };
    drop(st);
    issue_code(
        &pending.client_id,
        &pending.redirect_uri,
        &pending.challenge,
        pending.state.as_deref(),
        base,
    )
}

/// `McpHub remote approve CODE` (via l'API locale du démon).
pub fn approve_pairing(code: &str) -> Result<String, String> {
    let wanted = normalize_pairing(code);
    let mut st = state();
    st.prune(now());
    let client_id = {
        let pending = st
            .pending
            .values_mut()
            .find(|p| same(&p.code, &wanted))
            .ok_or("code inconnu ou expire")?;
        pending.approved = true;
        pending.client_id.clone()
    };
    st.sync()?;
    let label = st
        .store
        .clients
        .get(&client_id)
        .map(client_label)
        .unwrap_or(client_id);
    Ok(label)
}

// ─── Jetons ──────────────────────────────────────────────────

/// Identifiants du client sur /token et /revoke : Basic, ou client_id (+ client_secret) du corps.
fn client_credentials(
    req: &Request,
    form: &HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    if let Some(basic) = req.header("authorization").and_then(|h| {
        h.strip_prefix("Basic ")
            .or_else(|| h.strip_prefix("basic "))
    }) {
        if let Some(decoded) = b64_decode(basic.trim()) {
            if let Some((id, secret)) = String::from_utf8_lossy(&decoded).split_once(':') {
                return (
                    Some(super::http::urldecode(id)),
                    Some(super::http::urldecode(secret)),
                );
            }
        }
    }
    (
        form.get("client_id").cloned(),
        form.get("client_secret").cloned(),
    )
}

fn b64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn authenticate(client: &Client, secret: Option<&str>) -> bool {
    match (&client.secret_hash, secret) {
        (None, _) => true,
        (Some(hash), Some(secret)) => same(hash, &digest(secret)),
        (Some(_), None) => false,
    }
}

fn token_failure(ip: &str, status: u16, code: &str, description: &str) -> Response {
    // Seuls les échecs comptent : 5 par 15 minutes, puis refus en bloc.
    rate_allow(format!("token-fail:{}", ip), usize::MAX, 900);
    Response::oauth_error(status, code, description)
}

fn failures(ip: &str) -> usize {
    let mut st = state();
    let key = format!("token-fail:{}", ip);
    let window = std::time::Duration::from_secs(900);
    let now = Instant::now();
    let hits = st.hits.entry(key).or_default();
    while hits
        .front()
        .is_some_and(|t| now.duration_since(*t) > window)
    {
        hits.pop_front();
    }
    hits.len()
}

fn issue_tokens(st: &mut State, client_id: &str) -> Result<Value, String> {
    let access = random_token("mhr_")?;
    let refresh = random_token("mhf_")?;
    let issued = now();
    st.access.insert(
        digest(&access),
        Access {
            client_id: client_id.to_string(),
            expires: issued + ACCESS_TTL,
        },
    );
    let client = st
        .store
        .clients
        .get_mut(client_id)
        .ok_or("client revoque")?;
    client.refresh.push(RefreshGrant {
        hash: digest(&refresh),
        expires: issued + REFRESH_TTL,
    });
    // Un client se reconnecte rarement : 5 jetons de rafraîchissement actifs au plus.
    let excess = client.refresh.len().saturating_sub(5);
    client.refresh.drain(..excess);
    client.last_used = Some(issued);
    st.save()?;
    Ok(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL,
        "refresh_token": refresh,
        "scope": "mcp",
    }))
}

pub fn token(req: &Request) -> Response {
    let ip = req.client_ip();
    if failures(&ip) >= 5 {
        return Response::oauth_error(
            429,
            "slow_down",
            "too many failed attempts, wait 15 minutes",
        );
    }
    if !rate_allow(format!("token:{}", ip), 60, 60) {
        return Response::oauth_error(429, "slow_down", "too many requests");
    }
    let form = req.form();
    let (client_id, secret) = client_credentials(req, &form);
    let Some(client_id) = client_id else {
        return token_failure(&ip, 401, "invalid_client", "client_id is required");
    };
    let mut st = state();
    if let Err(e) = st.sync() {
        return Response::oauth_error(500, "server_error", &e);
    }
    st.prune(now());
    let Some(client) = st.store.clients.get(&client_id).cloned() else {
        drop(st);
        return token_failure(&ip, 401, "invalid_client", "unknown client");
    };
    if !authenticate(&client, secret.as_deref()) {
        drop(st);
        return token_failure(&ip, 401, "invalid_client", "client authentication failed");
    }
    let answer = match form.get("grant_type").map(|s| s.as_str()) {
        Some("authorization_code") => {
            let code = form.get("code").cloned().unwrap_or_default();
            // Usage unique : retiré avant toute vérification.
            let Some(stored) = st.codes.remove(&digest(&code)) else {
                drop(st);
                return token_failure(&ip, 400, "invalid_grant", "unknown or expired code");
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let redirect = form.get("redirect_uri").cloned();
            let redirect_ok = match &redirect {
                Some(r) => r == &stored.redirect_uri,
                None => client.redirect_uris.len() == 1,
            };
            if stored.client_id != client_id
                || stored.expires <= now()
                || !redirect_ok
                || verifier.len() < 43
                || verifier.len() > 128
                || !same(&crate::oauth::pkce_challenge(&verifier), &stored.challenge)
            {
                drop(st);
                return token_failure(
                    &ip,
                    400,
                    "invalid_grant",
                    "code, redirect_uri or code_verifier mismatch",
                );
            }
            issue_tokens(&mut st, &client_id)
        }
        Some("refresh_token") => {
            let presented = digest(form.get("refresh_token").map(|s| s.as_str()).unwrap_or(""));
            let entry = st.store.clients.get_mut(&client_id).and_then(|c| {
                let pos = c.refresh.iter().position(|r| same(&r.hash, &presented))?;
                Some(c.refresh.remove(pos))
            });
            match entry {
                Some(r) if r.expires > now() => issue_tokens(&mut st, &client_id),
                _ => {
                    drop(st);
                    return token_failure(
                        &ip,
                        400,
                        "invalid_grant",
                        "unknown, expired or revoked refresh_token",
                    );
                }
            }
        }
        _ => {
            drop(st);
            return Response::oauth_error(
                400,
                "unsupported_grant_type",
                "use authorization_code or refresh_token",
            );
        }
    };
    drop(st);
    match answer {
        Ok(value) => Response::json(200, &value).with("Pragma", "no-cache"),
        Err(e) => Response::oauth_error(400, "invalid_grant", &e),
    }
}

/// RFC 7009 : réponse 200 dans tous les cas, sauf client non authentifié.
pub fn revoke(req: &Request) -> Response {
    let form = req.form();
    let (client_id, secret) = client_credentials(req, &form);
    let presented = digest(form.get("token").map(|s| s.as_str()).unwrap_or(""));
    let mut st = state();
    if let Err(e) = st.sync() {
        return Response::oauth_error(500, "server_error", &e);
    }
    let Some(client_id) = client_id else {
        return Response::oauth_error(401, "invalid_client", "client_id is required");
    };
    match st.store.clients.get(&client_id) {
        Some(c) if authenticate(c, secret.as_deref()) => {}
        _ => return Response::oauth_error(401, "invalid_client", "client authentication failed"),
    }
    if st
        .access
        .get(&presented)
        .is_some_and(|a| a.client_id == client_id)
    {
        st.access.remove(&presented);
    }
    let changed = st
        .store
        .clients
        .get_mut(&client_id)
        .map(|c| {
            let before = c.refresh.len();
            c.refresh.retain(|r| !same(&r.hash, &presented));
            before != c.refresh.len()
        })
        .unwrap_or(false);
    if changed {
        if let Err(e) = st.save() {
            eprintln!("[McpHub][REMOTE] revocation : {}", e);
        }
    }
    Response::new(200, "application/json", b"{}".to_vec())
}

/// Client d'un jeton d'accès valide (libellé pour le journal d'audit), ou None.
pub fn check_access(bearer: &str) -> Option<(String, String)> {
    let hash = digest(bearer);
    let mut st = state();
    if st.sync().is_err() {
        return None;
    }
    let now = now();
    let client_id = match st.access.get(&hash) {
        Some(a) if a.expires > now => a.client_id.clone(),
        _ => return None,
    };
    let client = st.store.clients.get(&client_id)?;
    Some((client_id, client_label(client)))
}

// ─── Gestion (CLI, tableau de bord) ──────────────────────────

pub fn list_clients() -> Result<Vec<Client>, String> {
    let mut st = state();
    st.sync()?;
    Ok(st.store.clients.values().cloned().collect())
}

/// Supprime un client (ou tous avec "all") : ses jetons cessent de marcher aussitôt.
pub fn revoke_client(target: &str) -> Result<usize, String> {
    let mut st = state();
    st.sync()?;
    let removed = if target == "all" {
        let n = st.store.clients.len();
        st.store.clients.clear();
        n
    } else {
        usize::from(st.store.clients.remove(target).is_some())
    };
    if removed > 0 {
        st.save()?;
        let State { store, access, .. } = &mut *st;
        access.retain(|_, a| store.clients.contains_key(&a.client_id));
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_rules() {
        assert!(valid_redirect(
            "https://oauth-redirect.googleusercontent.com/r/abc"
        ));
        assert!(valid_redirect("https://claude.ai/api/mcp/auth_callback"));
        assert!(valid_redirect("http://127.0.0.1:33418/callback"));
        assert!(valid_redirect("http://localhost:8080/cb"));
        assert!(valid_redirect("com.example.app:/oauth"));
        assert!(!valid_redirect("http://evil.example.com/cb"));
        assert!(!valid_redirect("javascript:alert(1)"));
        assert!(!valid_redirect("https://a.example/cb#frag"));
        assert!(!valid_redirect("file:///etc/passwd"));
        assert!(!valid_redirect("not a url"));
    }

    #[test]
    fn redirect_keeps_existing_query() {
        assert_eq!(
            redirect_with("https://a.b/cb?x=1", &[("code", "a b"), ("state", "s&t")]),
            "https://a.b/cb?x=1&code=a%20b&state=s%26t"
        );
    }

    #[test]
    fn private_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "0.0.0.0",
        ] {
            assert!(forbidden_ip(ip.parse().unwrap()), "{}", ip);
        }
        for ip in ["1.1.1.1", "142.250.74.78", "2606:4700::1111"] {
            assert!(!forbidden_ip(ip.parse().unwrap()), "{}", ip);
        }
    }

    #[test]
    fn pairing_codes_are_readable() {
        let code = pairing_code().unwrap();
        assert_eq!(code.len(), 9);
        assert!(
            !code.contains('0')
                && !code.contains('O')
                && !code.contains('1')
                && !code.contains('I')
        );
        assert_eq!(
            normalize_pairing(&code.to_lowercase().replace('-', " ")),
            code
        );
    }

    #[test]
    fn basic_auth_decoding() {
        assert_eq!(
            b64_decode("Y2xpZW50OnNlY3JldA==").unwrap(),
            b"client:secret"
        );
        assert_eq!(b64_decode("YWI").unwrap(), b"ab");
        assert!(b64_decode("@@").is_none());
    }

    #[test]
    fn rate_window() {
        let mut st = State::default();
        for _ in 0..3 {
            assert!(st.allow("k".into(), 3, 60));
        }
        assert!(!st.allow("k".into(), 3, 60));
        assert!(st.allow("other".into(), 3, 60));
    }

    #[test]
    fn metadata_points_to_the_same_issuer() {
        let base = "https://hub.example.com";
        let rs = protected_resource_metadata(base);
        assert_eq!(rs["resource"], "https://hub.example.com/mcp");
        assert_eq!(rs["authorization_servers"][0], base);
        let asm = authorization_server_metadata(base);
        assert_eq!(asm["issuer"], base);
        assert_eq!(asm["code_challenge_methods_supported"][0], "S256");
    }
}
