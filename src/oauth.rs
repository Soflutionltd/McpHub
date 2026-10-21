//! OAuth 2.1 partagé pour les serveurs MCP distants (spec MCP 2025-06-18 / 2026-07-28).
//!
//! Remplace `mcp-remote` (un processus Node par serveur, jetons en clair dans ~/.mcp-auth) :
//! - découverte : métadonnées de ressource protégée (RFC 9728) puis du serveur d'autorisation
//!   (RFC 8414 / OpenID), avec repli sur les anciens chemins par défaut ;
//! - enregistrement dynamique du client (RFC 7591), PKCE S256, indicateur de ressource (RFC 8707) ;
//! - retour du navigateur sur le démon lui-même (http://localhost:24680/oauth/callback) ;
//! - jetons et client dans le Trousseau macOS, rafraîchis automatiquement ;
//! - import des connexions existantes de mcp-remote (aucune reconnexion à refaire).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CALLBACK_PATH: &str = "/oauth/callback";

pub fn redirect_uri() -> String {
    format!("http://localhost:{}{}", crate::dashboard::HTTP_PORT, CALLBACK_PATH)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AuthServerMeta {
    #[serde(default)]
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientInfo {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiration en millisecondes depuis l'époque Unix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Stored {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<AuthServerMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

pub const VAULT_PREFIX: &str = "MCPHUB_OAUTH_";

pub fn vault_name(server: &str) -> String {
    let norm: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("{}{}", VAULT_PREFIX, norm)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── Stockage (Trousseau, avec cache mémoire) ───────────────

fn cache() -> std::sync::MutexGuard<'static, HashMap<String, Stored>> {
    static C: OnceLock<Mutex<HashMap<String, Stored>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn load(server: &str) -> Option<Stored> {
    if let Some(s) = cache().get(server) {
        return Some(s.clone());
    }
    let raw = crate::vault::get(&vault_name(server)).ok().flatten()?;
    let s: Stored = serde_json::from_str(&raw).ok()?;
    cache().insert(server.to_string(), s.clone());
    Some(s)
}

pub fn save(server: &str, s: &Stored) -> Result<(), String> {
    let raw = serde_json::to_string(s).map_err(|e| e.to_string())?;
    crate::vault::set(&vault_name(server), &raw)?;
    cache().insert(server.to_string(), s.clone());
    Ok(())
}

pub fn forget(server: &str) -> Result<bool, String> {
    cache().remove(server);
    crate::vault::delete(&vault_name(server))
}

// ─── Outils ──────────────────────────────────────────────────

pub fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

fn random_b64(n: usize) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut buf).ok()?;
    Some(b64url(&buf))
}

pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("McpHub/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

/// Paramètre d'un en-tête WWW-Authenticate (`resource_metadata="..."`, `scope="..."`).
pub fn www_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let pat = format!("{}=", key);
    let mut search = 0;
    while let Some(pos) = lower[search..].find(&pat) {
        let i = search + pos;
        let ok_boundary = i == 0 || matches!(lower.as_bytes()[i - 1], b' ' | b',');
        let rest = &header[i + pat.len()..];
        if ok_boundary {
            return if let Some(r) = rest.strip_prefix('"') {
                r.find('"').map(|e| r[..e].to_string())
            } else {
                Some(rest.split([',', ' ']).next().unwrap_or("").to_string())
            };
        }
        search = i + pat.len();
    }
    None
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let r = client
        .get(url)
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2025-06-18")
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json::<Value>().await.ok()
}

/// HTTPS obligatoire (HTTP toléré seulement vers la machine locale).
pub fn secure_url(raw: &str) -> bool {
    match Url::parse(raw) {
        Ok(u) => match u.scheme() {
            "https" => true,
            "http" => matches!(u.host_str(), Some("localhost") | Some("127.0.0.1") | Some("[::1]")),
            _ => false,
        },
        Err(_) => false,
    }
}

/// La ressource annoncée doit couvrir l'URL du serveur (RFC 9728 §3.3) : même origine et
/// préfixe de chemin. Sinon un serveur pourrait obtenir un jeton destiné à un autre service.
pub fn resource_matches(resource: &str, url: &str) -> bool {
    let (Ok(r), Ok(u)) = (Url::parse(resource), Url::parse(url)) else {
        return false;
    };
    origin(&r) == origin(&u) && u.path().starts_with(r.path().trim_end_matches('/'))
}

fn origin(u: &Url) -> String {
    match u.port() {
        Some(p) => format!("{}://{}:{}", u.scheme(), u.host_str().unwrap_or(""), p),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")),
    }
}

/// Découvre le serveur d'autorisation et la ressource d'un serveur MCP.
pub async fn discover(url: &str, www_authenticate: Option<&str>) -> Result<(AuthServerMeta, Option<String>, Option<String>), String> {
    let client = http();
    let u = Url::parse(url).map_err(|e| format!("URL invalide : {}", e))?;
    let base = origin(&u);
    let path = u.path().trim_end_matches('/').to_string();

    let mut prm_urls = Vec::new();
    if let Some(h) = www_authenticate {
        if let Some(m) = www_param(h, "resource_metadata").filter(|m| secure_url(m)) {
            prm_urls.push(m);
        }
    }
    if !path.is_empty() {
        prm_urls.push(format!("{}/.well-known/oauth-protected-resource{}", base, path));
    }
    prm_urls.push(format!("{}/.well-known/oauth-protected-resource", base));

    let mut prm = None;
    for p in &prm_urls {
        if let Some(v) = get_json(&client, p).await {
            prm = Some(v);
            break;
        }
    }
    let mut scope = www_authenticate.and_then(|h| www_param(h, "scope"));
    let (issuer, resource) = match &prm {
        Some(v) => {
            let issuer = v
                .get("authorization_servers")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .map(String::from)
                .unwrap_or_else(|| base.clone());
            if scope.is_none() {
                scope = v
                    .get("scopes_supported")
                    .and_then(|s| s.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                    .filter(|s| !s.is_empty());
            }
            let resource = v.get("resource").and_then(|r| r.as_str()).map(String::from);
            if let Some(r) = &resource {
                if !resource_matches(r, url) {
                    return Err(format!("ressource annoncee ({}) sans rapport avec {} : refuse", r, url));
                }
            }
            (issuer, resource)
        }
        None => (base.clone(), None),
    };
    if !secure_url(&issuer) {
        return Err(format!("serveur d'autorisation non HTTPS refuse : {}", issuer));
    }

    let iu = Url::parse(&issuer).map_err(|e| format!("issuer invalide : {}", e))?;
    let ibase = origin(&iu);
    let ipath = iu.path().trim_end_matches('/').to_string();
    let mut meta_urls = Vec::new();
    if !ipath.is_empty() {
        meta_urls.push(format!("{}/.well-known/oauth-authorization-server{}", ibase, ipath));
        meta_urls.push(format!("{}/.well-known/openid-configuration{}", ibase, ipath));
        meta_urls.push(format!("{}{}/.well-known/openid-configuration", ibase, ipath));
    } else {
        meta_urls.push(format!("{}/.well-known/oauth-authorization-server", ibase));
        meta_urls.push(format!("{}/.well-known/openid-configuration", ibase));
    }
    for m in &meta_urls {
        if let Some(v) = get_json(&client, m).await {
            let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
            if let (Some(a), Some(t)) = (get("authorization_endpoint"), get("token_endpoint")) {
                let reg = get("registration_endpoint");
                if !secure_url(&a) || !secure_url(&t) || reg.as_deref().map_or(false, |r| !secure_url(r)) {
                    return Err("points d'acces OAuth non HTTPS refuses".into());
                }
                return Ok((
                    AuthServerMeta {
                        issuer: get("issuer").unwrap_or_else(|| issuer.clone()),
                        authorization_endpoint: a,
                        token_endpoint: t,
                        registration_endpoint: reg,
                    },
                    resource,
                    scope,
                ));
            }
        }
    }
    // Ancienne spec (2025-03-26) : chemins par défaut sur l'origine.
    Ok((
        AuthServerMeta {
            issuer: ibase.clone(),
            authorization_endpoint: format!("{}/authorize", ibase),
            token_endpoint: format!("{}/token", ibase),
            registration_endpoint: Some(format!("{}/register", ibase)),
        },
        resource,
        scope,
    ))
}

async fn register(meta: &AuthServerMeta) -> Result<ClientInfo, String> {
    let endpoint = meta
        .registration_endpoint
        .clone()
        .ok_or("le serveur n'accepte pas l'enregistrement dynamique de client")?;
    let body = serde_json::json!({
        "client_name": "McpHub",
        "redirect_uris": [redirect_uri()],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let r = http()
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("enregistrement du client : {}", e))?;
    let status = r.status();
    let v: Value = r.json().await.map_err(|e| format!("enregistrement du client : {}", e))?;
    if !status.is_success() {
        return Err(format!("enregistrement du client refuse ({}) : {}", status, v));
    }
    Ok(ClientInfo {
        client_id: v.get("client_id").and_then(|x| x.as_str()).ok_or("client_id absent")?.to_string(),
        client_secret: v.get("client_secret").and_then(|x| x.as_str()).map(String::from),
        token_endpoint_auth_method: v
            .get("token_endpoint_auth_method")
            .and_then(|x| x.as_str())
            .map(String::from),
        redirect_uri: Some(redirect_uri()),
    })
}

fn parse_tokens(v: &Value, previous_refresh: Option<String>) -> Result<Tokens, String> {
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("reponse sans access_token : {}", crate::redaction::redact_text(&v.to_string())))?;
    Ok(Tokens {
        access_token: access.to_string(),
        refresh_token: v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or(previous_refresh),
        expires_at: v.get("expires_in").and_then(|x| x.as_u64()).map(|s| now_ms() + s * 1000),
        scope: v.get("scope").and_then(|x| x.as_str()).map(String::from),
    })
}

async fn token_request(meta: &AuthServerMeta, client: &ClientInfo, mut form: Vec<(&str, String)>) -> Result<Value, String> {
    let mut req = http().post(&meta.token_endpoint).header("Accept", "application/json");
    let basic = client.client_secret.is_some()
        && client.token_endpoint_auth_method.as_deref() != Some("client_secret_post");
    if basic {
        req = req.basic_auth(&client.client_id, client.client_secret.as_deref());
    } else {
        form.push(("client_id", client.client_id.clone()));
        if let Some(sec) = &client.client_secret {
            form.push(("client_secret", sec.clone()));
        }
    }
    let r = req.form(&form).send().await.map_err(|e| format!("serveur de jetons : {}", e))?;
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("jeton refuse ({}) : {}", status, crate::redaction::redact_text(&text.chars().take(300).collect::<String>())));
    }
    Ok(v)
}

/// Jeton d'accès valide (rafraîchi si besoin). `None` = connexion requise.
pub async fn access_token(server: &str, url: &str) -> Result<Option<String>, String> {
    let lock = server_lock(server);
    let _g = lock.lock().await;
    let Some(mut s) = load(server) else { return Ok(None) };
    if !same_target(&s.url, url) {
        // Jeton émis pour une autre adresse : ne jamais l'envoyer ailleurs.
        return Ok(None);
    }
    let Some(tokens) = s.tokens.clone() else { return Ok(None) };
    let fresh = tokens.expires_at.map_or(true, |e| e > now_ms() + 60_000);
    if fresh {
        return Ok(Some(tokens.access_token));
    }
    refresh_locked(server, url, &mut s).await.map(Some)
}

/// Rafraîchit après un 401 (jeton révoqué avant son expiration annoncée).
pub async fn force_refresh(server: &str, url: &str) -> Result<Option<String>, String> {
    let lock = server_lock(server);
    let _g = lock.lock().await;
    let Some(mut s) = load(server) else { return Ok(None) };
    if !same_target(&s.url, url) {
        return Ok(None);
    }
    if s.tokens.as_ref().and_then(|t| t.refresh_token.as_ref()).is_none() {
        return Ok(None);
    }
    refresh_locked(server, url, &mut s).await.map(Some)
}

fn same_target(stored: &str, url: &str) -> bool {
    stored.trim_end_matches('/') == url.trim_end_matches('/')
}

async fn refresh_locked(server: &str, url: &str, s: &mut Stored) -> Result<String, String> {
    let tokens = s.tokens.clone().ok_or("aucun jeton")?;
    let refresh = tokens.refresh_token.clone().ok_or("jeton expire et pas de refresh_token")?;
    let client = s.client.clone().ok_or("client OAuth inconnu")?;
    if s.meta.is_none() {
        let (meta, resource, scope) = discover(url, None).await?;
        s.meta = Some(meta);
        s.resource = s.resource.clone().or(resource);
        s.scope = s.scope.clone().or(scope);
    }
    let meta = s.meta.clone().unwrap_or_default();
    let mut form = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh.clone())];
    if let Some(r) = &s.resource {
        form.push(("resource", r.clone()));
    }
    let v = token_request(&meta, &client, form).await?;
    let new = parse_tokens(&v, Some(refresh))?;
    let access = new.access_token.clone();
    s.tokens = Some(new);
    save(server, s)?;
    eprintln!("[McpHub][OAUTH] {} : jeton rafraichi", server);
    Ok(access)
}

fn server_lock(server: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static L: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(server.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// ─── Connexion interactive ───────────────────────────────────

type CallbackResult = Result<String, String>;

fn pending() -> std::sync::MutexGuard<'static, HashMap<String, tokio::sync::oneshot::Sender<CallbackResult>>> {
    static P: OnceLock<Mutex<HashMap<String, tokio::sync::oneshot::Sender<CallbackResult>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Appelé par la route /oauth/callback du démon. Retourne vrai si l'état était attendu.
pub fn complete_callback(state: &str, result: CallbackResult) -> bool {
    match pending().remove(state) {
        Some(tx) => tx.send(result).is_ok(),
        None => false,
    }
}

/// Connexion complète dans le navigateur. Bloque jusqu'au retour (5 minutes au plus).
fn logins_in_progress() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    static S: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct LoginGuard(String);
impl Drop for LoginGuard {
    fn drop(&mut self) {
        logins_in_progress().remove(&self.0);
    }
}

pub async fn interactive_login(server: &str, url: &str, www_authenticate: Option<&str>) -> Result<(), String> {
    // Une seule connexion à la fois par serveur, et sans bloquer ses appels en attendant le
    // navigateur (le verrou du serveur n'est pris qu'à l'enregistrement final).
    if !logins_in_progress().insert(server.to_string()) {
        return Err("une connexion est deja en cours pour ce serveur".into());
    }
    let _login = LoginGuard(server.to_string());
    let (meta, resource, scope) = discover(url, www_authenticate).await?;
    let mut stored = load(server).unwrap_or_default();
    stored.url = url.to_string();
    let reuse = stored.client.as_ref().filter(|c| {
        c.redirect_uri.as_deref() == Some(redirect_uri().as_str())
            && stored.meta.as_ref().map(|m| m.issuer == meta.issuer).unwrap_or(false)
    });
    let client = match reuse {
        Some(c) => c.clone(),
        None => register(&meta).await?,
    };
    let verifier = random_b64(48).ok_or("source aleatoire indisponible")?;
    let state = random_b64(24).ok_or("source aleatoire indisponible")?;
    if !secure_url(&meta.authorization_endpoint) {
        return Err("page de connexion non HTTPS refusee".into());
    }
    let mut auth = Url::parse(&meta.authorization_endpoint).map_err(|e| e.to_string())?;
    {
        let mut q = auth.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri())
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        if let Some(sc) = scope.as_ref().or(stored.scope.as_ref()) {
            q.append_pair("scope", sc);
        }
        if let Some(r) = &resource {
            q.append_pair("resource", r);
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending().insert(state.clone(), tx);
    eprintln!("[McpHub][OAUTH] {} : connexion ouverte dans le navigateur", server);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(auth.as_str()).spawn();
    #[cfg(not(target_os = "macos"))]
    eprintln!("[McpHub][OAUTH] Ouvrir : {}", auth);
    let code = match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(Ok(code))) => code,
        Ok(Ok(Err(e))) => return Err(format!("connexion refusee : {}", e)),
        _ => {
            pending().remove(&state);
            return Err("connexion non terminee dans les 5 minutes".into());
        }
    };
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri()),
        ("code_verifier", verifier),
    ];
    if let Some(r) = &resource {
        form.push(("resource", r.clone()));
    }
    let v = token_request(&meta, &client, form).await?;
    let lock = server_lock(server);
    let _g = lock.lock().await;
    stored.tokens = Some(parse_tokens(&v, None)?);
    stored.client = Some(client);
    stored.meta = Some(meta);
    stored.resource = resource;
    stored.scope = scope.or(stored.scope);
    save(server, &stored)?;
    eprintln!("[McpHub][OAUTH] {} : connecte", server);
    Ok(())
}

// ─── Import depuis mcp-remote ────────────────────────────────

/// Reprend client et jetons de mcp-remote (~/.mcp-auth/mcp-remote-*/<md5(url)>_*.json).
pub fn import_mcp_remote(server: &str, url: &str) -> Result<bool, String> {
    use md5::{Digest, Md5};
    let hash: String = Md5::digest(url.as_bytes()).iter().map(|b| format!("{:02x}", b)).collect();
    let root = dirs::home_dir().ok_or("dossier personnel introuvable")?.join(".mcp-auth");
    let mut dirs_found: Vec<_> = std::fs::read_dir(&root)
        .map_err(|e| format!("{} : {}", root.display(), e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("mcp-remote")))
        .collect();
    // Les plus récents d'abord.
    dirs_found.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p.join(format!("{}_tokens.json", hash))).and_then(|m| m.modified()).ok()));
    for dir in dirs_found {
        let tok_path = dir.join(format!("{}_tokens.json", hash));
        let cli_path = dir.join(format!("{}_client_info.json", hash));
        let (Ok(tok), Ok(cli)) = (std::fs::read_to_string(&tok_path), std::fs::read_to_string(&cli_path)) else {
            continue;
        };
        let tv: Value = serde_json::from_str(&tok).map_err(|e| e.to_string())?;
        let cv: Value = serde_json::from_str(&cli).map_err(|e| e.to_string())?;
        let mut tokens = parse_tokens(&tv, None)?;
        if let Some(e) = tv.get("expires_at").and_then(|x| x.as_u64()) {
            tokens.expires_at = Some(e);
        }
        let client = ClientInfo {
            client_id: cv.get("client_id").and_then(|x| x.as_str()).ok_or("client_id absent")?.to_string(),
            client_secret: cv.get("client_secret").and_then(|x| x.as_str()).map(String::from),
            token_endpoint_auth_method: cv.get("token_endpoint_auth_method").and_then(|x| x.as_str()).map(String::from),
            redirect_uri: cv
                .get("redirect_uris")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.as_str())
                .map(String::from),
        };
        let stored = Stored {
            url: url.to_string(),
            scope: tv.get("requested_scope").and_then(|x| x.as_str()).map(String::from),
            client: Some(client),
            tokens: Some(tokens),
            ..Default::default()
        };
        save(server, &stored)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_et_pkce() {
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        // Exemple de la RFC 7636, annexe B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn en_tete_www_authenticate() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://mcp.x.com/.well-known/oauth-protected-resource", scope="a b""#;
        assert_eq!(
            www_param(h, "resource_metadata").as_deref(),
            Some("https://mcp.x.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(www_param(h, "scope").as_deref(), Some("a b"));
        assert_eq!(www_param(h, "nope"), None);
    }

    #[test]
    fn nom_dans_le_trousseau() {
        assert_eq!(vault_name("tiktok-ads"), "MCPHUB_OAUTH_TIKTOK_ADS");
        assert!(crate::vault::is_valid_name(&vault_name("cloudflare.api")));
    }

    #[test]
    fn urls_sures_et_ressource() {
        assert!(secure_url("https://a.com/x"));
        assert!(secure_url("http://localhost:3000/cb"));
        assert!(!secure_url("http://a.com/x"));
        assert!(!secure_url("file:///Applications/Evil.app"));
        assert!(!secure_url("smb://host/share"));
        assert!(resource_matches("https://mcp.sentry.dev/mcp", "https://mcp.sentry.dev/mcp"));
        assert!(resource_matches("https://mcp.sentry.dev", "https://mcp.sentry.dev/mcp"));
        assert!(!resource_matches("https://api.github.com", "https://mcp.sentry.dev/mcp"));
        assert!(same_target("https://a.com/mcp/", "https://a.com/mcp"));
        assert!(!same_target("https://a.com/mcp", "https://b.com/mcp"));
    }

    #[test]
    fn options_mcp_remote_non_prises_en_charge() {
        let ok = serde_json::json!(["-y", "mcp-remote@0.15.1", "https://x.com/mcp", "--header", "Authorization: Bearer ${T}"]);
        let (u, h) = mcp_remote_args(ok.as_array().unwrap()).unwrap();
        assert_eq!(u, "https://x.com/mcp");
        assert_eq!(h[0].0, "Authorization");
        let ko = serde_json::json!(["-y", "mcp-remote", "https://x.com/mcp", "--transport", "sse-only"]);
        assert!(mcp_remote_args(ko.as_array().unwrap()).is_none());
    }

    #[test]
    fn jetons() {
        let v = serde_json::json!({"access_token":"a","expires_in":60,"scope":"x"});
        let t = parse_tokens(&v, Some("r".into())).unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        assert!(t.expires_at.unwrap() > now_ms());
        assert!(parse_tokens(&serde_json::json!({}), None).is_err());
    }
}

// ─── Commande `McpHub oauth` ─────────────────────────────────

fn remote_servers() -> Vec<(String, crate::config::ServerConfig)> {
    let cfg = crate::config::auto_detect();
    let mut v: Vec<_> = cfg.servers.into_iter().filter(|(_, s)| s.url.is_some()).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn describe(s: &Stored) -> String {
    match &s.tokens {
        None => "pas de jeton".into(),
        Some(t) => {
            let exp = match t.expires_at {
                Some(e) if e > now_ms() => format!("expire dans {} min", (e - now_ms()) / 60_000),
                Some(_) => "expire (sera rafraichi)".into(),
                None => "sans expiration".into(),
            };
            format!(
                "connecte, {}{}",
                exp,
                if t.refresh_token.is_some() { ", rafraichissable" } else { "" }
            )
        }
    }
}

/// Recherche les arguments d'un serveur mcp-remote dans la config brute : (url, en-têtes).
fn mcp_remote_args(args: &[Value]) -> Option<(String, Vec<(String, String)>)> {
    let strs: Vec<&str> = args.iter().filter_map(|a| a.as_str()).collect();
    if !strs.iter().any(|a| a.contains("mcp-remote")) {
        return None;
    }
    // Options de mcp-remote que McpHub ne reproduit pas : on ne migre pas ce serveur.
    if strs
        .iter()
        .any(|a| a.starts_with("--") && *a != "--header" && !a.starts_with("--header="))
    {
        eprintln!("[McpHub] migration ignoree : options mcp-remote non prises en charge ({})", strs.join(" "));
        return None;
    }
    let url = strs.iter().find(|a| a.starts_with("http://") || a.starts_with("https://"))?.to_string();
    let mut headers = Vec::new();
    let mut i = 0;
    while i < strs.len() {
        if strs[i] == "--header" {
            if let Some(h) = strs.get(i + 1) {
                if let Some((k, v)) = h.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    Some((url, headers))
}

pub async fn run_cli(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()).unwrap_or("status") {
        "status" => {
            let servers = remote_servers();
            if servers.is_empty() {
                println!("Aucun serveur distant natif (url) dans la config.");
            }
            for (name, sc) in servers {
                let state = if !sc.oauth {
                    "en-tetes fixes (pas d'OAuth)".to_string()
                } else {
                    load(&name).map(|s| describe(&s)).unwrap_or_else(|| "non connecte : McpHub oauth login".into())
                };
                println!("{:24} {}", name, state);
            }
            0
        }
        "login" => {
            let Some(server) = args.get(1) else {
                eprintln!("Usage : McpHub oauth login SERVEUR");
                return 2;
            };
            let token = crate::dashboard::get_auth_token();
            let url = format!("http://127.0.0.1:{}/api/oauth/login", crate::dashboard::HTTP_PORT);
            let r = reqwest::Client::new()
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({ "server": server }))
                .send()
                .await;
            match r {
                Ok(r) if r.status().is_success() => {
                    println!("Connexion ouverte dans le navigateur pour {}. En attente (5 min max)...", server);
                    let before = load(server).and_then(|s| s.tokens).map(|t| t.access_token);
                    for _ in 0..150 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        cache().remove(server.as_str());
                        let now = load(server).and_then(|s| s.tokens).map(|t| t.access_token);
                        if now.is_some() && now != before {
                            println!("{} : connecte.", server);
                            return 0;
                        }
                    }
                    eprintln!("Pas de connexion terminee. Voir : McpHub logs");
                    1
                }
                Ok(r) => {
                    eprintln!("Refuse par le demon : {}", r.text().await.unwrap_or_default());
                    1
                }
                Err(e) => {
                    eprintln!("Demon McpHub injoignable ({}). Il doit tourner (McpHub install).", e);
                    1
                }
            }
        }
        "logout" => {
            let Some(server) = args.get(1) else {
                eprintln!("Usage : McpHub oauth logout SERVEUR");
                return 2;
            };
            // Passe par le démon pour qu'il oublie aussi sa copie en mémoire.
            let token = crate::dashboard::get_auth_token();
            let url = format!("http://127.0.0.1:{}/api/oauth/logout", crate::dashboard::HTTP_PORT);
            let via_daemon = reqwest::Client::new()
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({ "server": server }))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            let local = forget(server);
            match (via_daemon, local) {
                (_, Err(e)) => {
                    eprintln!("{}", e);
                    1
                }
                (true, _) | (_, Ok(true)) => {
                    println!("{} : deconnecte (jetons supprimes du Trousseau).", server);
                    0
                }
                (false, Ok(false)) => {
                    println!("{} : aucun jeton.", server);
                    0
                }
            }
        }
        "migrate" => {
            // Convertit les serveurs `npx mcp-remote URL` en serveurs distants natifs, en
            // reprenant leurs jetons. Sauvegarde de config.json avant toute écriture.
            let dry = !args.iter().any(|a| a == "--apply");
            let only: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
            let path = dirs::home_dir().unwrap_or_default().join(".McpHub/config.json");
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{} : {}", path.display(), e);
                    return 1;
                }
            };
            let mut json: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("config.json illisible : {}", e);
                    return 1;
                }
            };
            let key = if json.get("mcpServers").is_some() { "mcpServers" } else { "servers" };
            let Some(servers) = json.get_mut(key).and_then(|v| v.as_object_mut()) else {
                eprintln!("Aucun serveur.");
                return 1;
            };
            let mut changed = 0;
            for (name, entry) in servers.iter_mut() {
                if !only.is_empty() && !only.iter().any(|o| *o == name) {
                    continue;
                }
                let Some((url, headers)) = entry.get("args").and_then(|a| a.as_array()).and_then(|a| mcp_remote_args(a)) else {
                    continue;
                };
                let has_auth = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                let imported = if has_auth {
                    "en-tete Authorization conserve".to_string()
                } else if dry {
                    "jetons mcp-remote a importer".to_string()
                } else {
                    match import_mcp_remote(name, &url) {
                        Ok(true) => "jetons mcp-remote importes au Trousseau".into(),
                        Ok(false) => "aucun jeton mcp-remote : McpHub oauth login necessaire".into(),
                        Err(e) => format!("import impossible ({}) : McpHub oauth login necessaire", e),
                    }
                };
                println!("{:24} {} -> {}", name, url, imported);
                if dry {
                    continue;
                }
                let obj = entry.as_object_mut().expect("objet");
                let backup = Value::Object(obj.clone());
                obj.remove("command");
                obj.remove("args");
                obj.insert("url".into(), Value::String(url));
                if !headers.is_empty() {
                    let h: serde_json::Map<String, Value> =
                        headers.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
                    obj.insert("headers".into(), Value::Object(h));
                }
                obj.insert("auth".into(), Value::String(if has_auth { "none".into() } else { "oauth".into() }));
                obj.insert("_mcpRemote".into(), backup);
                if let Some(env) = obj.get_mut("env").and_then(|e| e.as_object_mut()) {
                    env.remove("PATH");
                }
                changed += 1;
            }
            if dry {
                println!("\nApercu seulement. Appliquer : McpHub oauth migrate [SERVEUR...] --apply");
                return 0;
            }
            if changed == 0 {
                println!("Rien a migrer.");
                return 0;
            }
            let backup_path = path.with_extension(format!("json.avant-oauth-{}", now_ms() / 1000));
            if let Err(e) = std::fs::write(&backup_path, &raw) {
                eprintln!("Sauvegarde impossible : {}", e);
                return 1;
            }
            crate::vault::restrict_permissions(&backup_path);
            match std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap_or_default()) {
                Ok(_) => {
                    println!(
                        "\n{} serveur(s) migre(s). Sauvegarde : {}\nRetour arriere d'un serveur : remettre son bloc _mcpRemote.",
                        changed,
                        backup_path.display()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("Ecriture impossible : {}", e);
                    1
                }
            }
        }
        _ => {
            eprintln!("Usage : McpHub oauth [status | login SERVEUR | logout SERVEUR | migrate [SERVEUR...] [--apply]]");
            2
        }
    }
}
