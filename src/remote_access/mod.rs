//! Accès distant : Gemini, claude.ai, ChatGPT (ou tout client MCP distant) joignent McpHub
//! depuis Internet, par OAuth, sans voir autre chose que le profil `remote`.
//!
//! Deux transports, une seule porte d'entrée (`handle`) :
//! - **tunnel** : `cloudflared` relaie `https://<domaine>` vers le socket Unix
//!   `~/.McpHub/remote/tunnel.sock` (dossier en 700), jamais vers un port TCP : ni page web ni
//!   autre processus ne l'atteint. Il ne sert que `/mcp`, l'OAuth et la découverte : jamais le
//!   tableau de bord ni l'API `/api`.
//! - **relais** : le démon ouvre un WebSocket sortant vers un relais Cloudflare (dossier
//!   `relay/`), qui lui transmet les requêtes reçues sur `https://<hubId>.<domaine du relais>`.
//!
//! Sécurité (refus par défaut) :
//! - le profil `remote` doit exister, nommer ses serveurs et être en lecture seule, sinon tout
//!   est refusé (`toolguard::remote_profile_problem`) ;
//! - l'en-tête Host doit être celui de l'adresse publique configurée (anti DNS rebinding) ;
//! - chaque connexion d'un client est validée sur la machine (fenêtre ou code d'appairage) ;
//! - débits limités : 100 appels MCP par minute et par client, inscriptions, autorisations et
//!   échecs de jeton limités par adresse ;
//! - chaque appel est noté au journal d'audit avec le client distant.
//!
//! Réglages : `settings.remoteAccess` dans config.json, écrits par `McpHub remote enable`.

pub mod authz;
pub mod http;
pub mod relay;
pub mod tunnel;

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::proxy::ProxyServer;
use http::{Request, Response, MAX_BODY};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tunnel,
    Relay,
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Tunnel => "tunnel",
            Transport::Relay => "relay",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub transport: Transport,
    /// Adresse publique, sans barre finale (ex. https://hub.example.com).
    pub public_url: String,
    /// Relais : adresse https du relais (ex. https://relay.example.com).
    pub relay_url: Option<String>,
}

pub fn remote_dir() -> std::path::PathBuf {
    crate::instance::hub_dir().join("remote")
}

/// Crée `~/.McpHub/remote` en mode 700 (dossier : le bit d'exécution est nécessaire pour y
/// créer des fichiers ; ceux-ci sont ensuite en 600).
pub fn ensure_private_dir() -> Result<std::path::PathBuf, String> {
    let dir = remote_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} : {}", dir.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{} : {}", dir.display(), e))?;
    }
    Ok(dir)
}

/// Socket du tunnel : `cloudflared` s'y connecte ; dans un dossier en 700, aucun autre
/// utilisateur, aucune page web et aucun serveur isolé ne l'atteint.
pub fn socket_path() -> std::path::PathBuf {
    remote_dir().join("tunnel.sock")
}

pub fn config_path() -> std::path::PathBuf {
    crate::instance::hub_dir().join("config.json")
}

impl Settings {
    pub fn from_json(value: Option<&Value>) -> Settings {
        let get = |k: &str| value.and_then(|v| v.get(k));
        let transport = match get("transport").and_then(|v| v.as_str()) {
            Some("relay") => Transport::Relay,
            _ => Transport::Tunnel,
        };
        Settings {
            enabled: get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            transport,
            public_url: get("publicUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string(),
            relay_url: get("relayUrl")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_end_matches('/').to_string()),
        }
    }

    pub fn load() -> Result<Settings, String> {
        let config = crate::config::read_raw(&config_path())?;
        Ok(Settings::from_json(
            config
                .as_ref()
                .and_then(|c| c.get("settings"))
                .and_then(|s| s.get("remoteAccess")),
        ))
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "enabled": self.enabled,
            "transport": self.transport.as_str(),
            "publicUrl": self.public_url,
        });
        if let Some(r) = &self.relay_url {
            v["relayUrl"] = json!(r);
        }
        v
    }

    /// Hôte de l'adresse publique, si elle est une adresse https valide.
    pub fn public_host(&self) -> Option<String> {
        let url = reqwest::Url::parse(&self.public_url).ok()?;
        if url.scheme() != "https" || url.path() != "/" || url.query().is_some() {
            return None;
        }
        url.host_str().map(|h| h.to_ascii_lowercase())
    }
}

/// Écrit `settings.remoteAccess` (et crée le profil `remote` s'il manque, vide et en lecture
/// seule : rien n'est exposé tant qu'aucun serveur n'y est ajouté).
pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = config_path();
    let mut config = crate::config::read_raw(&path)?
        .unwrap_or_else(|| json!({"mcpServers": {}, "settings": {}}));
    let root = config
        .as_object_mut()
        .ok_or("config.json n'est pas un objet")?;
    let settings_obj = root
        .entry("settings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("settings n'est pas un objet")?;
    settings_obj.insert("remoteAccess".into(), settings.to_json());
    let profiles = settings_obj
        .entry("profiles")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("settings.profiles n'est pas un objet")?;
    profiles
        .entry(crate::toolguard::REMOTE_PROFILE)
        .or_insert_with(|| json!({"servers": [], "readOnly": true}));
    crate::config::write_raw_atomic(&path, &config)
}

/// Ajoute ou retire un serveur du profil `remote` (toujours en lecture seule).
pub fn set_profile_server(server: &str, allowed: bool) -> Result<Vec<String>, String> {
    let path = config_path();
    let mut config = crate::config::read_raw(&path)?.ok_or("config.json introuvable")?;
    // Même ordre que config.rs : "mcpServers", sinon "servers".
    let known = config
        .get("mcpServers")
        .or_else(|| config.get("servers"))
        .and_then(|m| m.get(server))
        .is_some();
    if allowed && !known {
        return Err(format!("serveur inconnu dans config.json : {}", server));
    }
    let profile = config
        .as_object_mut()
        .ok_or("config.json n'est pas un objet")?
        .entry("settings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("settings n'est pas un objet")?
        .entry("profiles")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("settings.profiles n'est pas un objet")?
        .entry(crate::toolguard::REMOTE_PROFILE)
        .or_insert_with(|| json!({"servers": [], "readOnly": true}))
        .as_object_mut()
        .ok_or("profil remote invalide")?;
    profile.insert("readOnly".into(), json!(true));
    let mut servers: Vec<String> = profile
        .get("servers")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    servers.retain(|s| s != server);
    if allowed {
        servers.push(server.to_string());
    }
    servers.sort();
    profile.insert("servers".into(), json!(servers));
    crate::config::write_raw_atomic(&path, &config)?;
    Ok(servers)
}

// ─── Porte d'entrée commune ──────────────────────────────────

/// Traite une requête venue d'Internet (tunnel ou relais) et rend la réponse HTTP complète.
pub async fn handle(proxy: &Arc<ProxyServer>, settings: &Settings, req: Request) -> Vec<u8> {
    let Some(public_host) = settings.public_host() else {
        return Response::text(503, "remote access is not configured").to_bytes();
    };
    if req.host().as_deref() != Some(public_host.as_str()) {
        eprintln!(
            "[McpHub][REMOTE] Hote refuse : {:?} (attendu {})",
            req.host(),
            public_host
        );
        return Response::text(421, "unknown host").to_bytes();
    }
    let base = settings.public_url.as_str();
    let path = req.path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let response = match (req.method.as_str(), path) {
        ("GET", "/.well-known/oauth-protected-resource")
        | ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            Response::json(200, &authz::protected_resource_metadata(base))
        }
        ("GET", "/.well-known/oauth-authorization-server")
        | ("GET", "/.well-known/openid-configuration") => {
            Response::json(200, &authz::authorization_server_metadata(base))
        }
        ("POST", "/register") => authz::register(&req),
        ("GET", "/authorize") => authz::authorize(&req, base).await,
        ("GET", "/authorize/continue") => authz::authorize_continue(&req, base),
        ("POST", "/token") => authz::token(&req),
        ("POST", "/revoke") => authz::revoke(&req),
        ("POST", "/mcp") => return handle_mcp(proxy, base, req).await,
        ("GET", "/mcp") | ("DELETE", "/mcp") => Response::json(
            405,
            &json!({"error": "Streamable HTTP sans etat : utiliser POST"}),
        )
        .with("Allow", "POST"),
        ("GET", "/") => Response::text(200, "McpHub remote access"),
        _ => Response::text(404, "not found"),
    };
    response.to_bytes()
}

const MAX_BATCH: usize = 20;

fn unauthorized(base: &str, invalid: bool) -> Vec<u8> {
    let mut challenge = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"mcp\"",
        base
    );
    if invalid {
        challenge.push_str(", error=\"invalid_token\"");
    }
    Response::json(401, &json!({"error": "invalid_token"}))
        .with("WWW-Authenticate", &challenge)
        .to_bytes()
}

async fn handle_mcp(proxy: &Arc<ProxyServer>, base: &str, req: Request) -> Vec<u8> {
    let bearer = req
        .header("authorization")
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_string());
    let Some(bearer) = bearer else {
        return unauthorized(base, false);
    };
    let Some((client_id, label)) = authz::check_access(&bearer) else {
        return unauthorized(base, true);
    };
    if let Some(problem) = crate::toolguard::remote_profile_problem() {
        return Response::json(
            503,
            &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32001, "message": format!("acces distant ferme : {}", problem)}}),
        )
        .to_bytes();
    }
    // Un lot compte pour autant d'appels qu'il contient, 20 au plus.
    let calls = match serde_json::from_slice::<Value>(&req.body) {
        Ok(Value::Array(items)) => items.len(),
        _ => 1,
    };
    if calls > MAX_BATCH {
        return Response::json(
            400,
            &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("lot trop grand : {} requetes au plus", MAX_BATCH)}}),
        )
        .to_bytes();
    }
    for _ in 0..calls.max(1) {
        if !authz::rate_allow(format!("mcp:{}", client_id), 100, 60) {
            return Response::json(
                429,
                &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32002, "message": "trop d'appels : 100 par minute au plus"}}),
            )
            .with("Retry-After", "60")
            .to_bytes();
        }
    }
    let body = String::from_utf8_lossy(&req.body).to_string();
    crate::dashboard::handle_streamable_http_message(&body, proxy, None, Some(label)).await
}

// ─── Démarrage dans le démon ─────────────────────────────────

/// Lance le transport configuré. Sans réglage valide, rien n'écoute.
pub fn spawn(proxy: Arc<ProxyServer>) {
    let settings = match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[McpHub][REMOTE] Reglages illisibles, acces distant ferme : {}",
                e
            );
            return;
        }
    };
    if !settings.enabled {
        return;
    }
    if settings.public_host().is_none() {
        eprintln!(
            "[McpHub][REMOTE] publicUrl invalide ({:?}) : acces distant ferme",
            settings.public_url
        );
        return;
    }
    if let Some(problem) = crate::toolguard::remote_profile_problem() {
        eprintln!(
            "[McpHub][REMOTE] {} (les requetes seront refusees tant que le profil n'est pas corrige)",
            problem
        );
    }
    let settings = Arc::new(settings);
    match settings.transport {
        Transport::Tunnel => {
            tokio::spawn(serve_local(proxy, settings));
        }
        Transport::Relay => {
            tokio::spawn(relay::run(proxy, settings));
        }
    }
}

/// Connexions traitées en même temps sur le socket du tunnel.
const MAX_CONNECTIONS: usize = 64;
/// Corps admis hors de `/mcp` (inscription, jeton, révocation).
const MAX_SMALL_BODY: usize = 64 * 1024;

/// Socket du tunnel (Unix uniquement : le tunnel n'existe que sur macOS et Linux).
#[cfg(unix)]
async fn serve_local(proxy: Arc<ProxyServer>, settings: Arc<Settings>) {
    let path = socket_path();
    if let Err(e) = ensure_private_dir() {
        eprintln!("[McpHub][REMOTE] {}", e);
        return;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[McpHub][REMOTE] Socket {} indisponible : {}",
                path.display(),
                e
            );
            return;
        }
    };
    crate::vault::restrict_permissions(&path);
    eprintln!(
        "[McpHub][REMOTE] Acces distant pret : {} -> {}",
        settings.public_url,
        path.display()
    );
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let proxy = proxy.clone();
        let settings = settings.clone();
        tokio::spawn(async move {
            serve_connection(stream, &proxy, &settings).await;
            drop(permit);
        });
    }
}

#[cfg(not(unix))]
async fn serve_local(_proxy: Arc<ProxyServer>, _settings: Arc<Settings>) {
    eprintln!("[McpHub][REMOTE] Tunnel : macOS et Linux seulement dans cette version");
}

/// Lit une requête entière en 30 s au plus (pas de connexion lente qui s'éternise), corps
/// limité à 16 Mo pour `/mcp` et 64 Ko ailleurs, puis répond.
async fn serve_connection<S>(mut stream: S, proxy: &Arc<ProxyServer>, settings: &Settings)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = vec![0u8; 65536];
    let (mut req, head_len, body_len) = loop {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(parsed) = http::parse_head(&buf) {
            break parsed;
        }
        if buf.len() > 64 * 1024 || buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let _ = stream
                .write_all(&Response::text(400, "bad request").to_bytes())
                .await;
            return;
        }
    };
    let limit = if req.path.trim_end_matches('/') == "/mcp" {
        MAX_BODY
    } else {
        MAX_SMALL_BODY
    };
    if body_len > limit {
        let _ = stream
            .write_all(&Response::text(413, "payload too large").to_bytes())
            .await;
        return;
    }
    while buf.len() < head_len + body_len {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        buf.extend_from_slice(&chunk[..n]);
    }
    req.body = buf[head_len..head_len + body_len].to_vec();
    let response = handle(proxy, settings, req).await;
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

// ─── API locale (tableau de bord, CLI) ───────────────────────

pub fn api_status() -> Value {
    let settings = Settings::load();
    let clients = authz::list_clients().unwrap_or_default();
    json!({
        "settings": settings.as_ref().map(|s| s.to_json()).unwrap_or(Value::Null),
        "settingsError": settings.err(),
        "profileProblem": crate::toolguard::remote_profile_problem(),
        "servers": crate::toolguard::remote_profile_servers(),
        "clients": clients.iter().map(|c| json!({
            "clientId": c.client_id,
            "name": c.name,
            "kind": c.kind,
            "redirectUris": c.redirect_uris,
            "created": c.created,
            "approvedAt": c.approved_at,
            "lastUsed": c.last_used,
        })).collect::<Vec<_>>(),
    })
}

/// `{"code"}` décrit la demande ; `{"code", "confirm": true}` la valide.
pub fn api_approve(body: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body).map_err(|_| "corps JSON attendu")?;
    let code = parsed
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or("champ code manquant")?;
    let confirm = parsed.get("confirm").and_then(|c| c.as_bool()) == Some(true);
    authz::approve_pairing(code, confirm)
}

pub fn api_revoke(body: &str) -> Result<usize, String> {
    let target = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("clientId").and_then(|c| c.as_str()).map(String::from))
        .ok_or("champ clientId manquant")?;
    authz::revoke_client(&target)
}

// ─── CLI : McpHub remote … ───────────────────────────────────

fn print_usage() {
    println!(
        "McpHub remote : acces depuis Internet (Gemini, claude.ai, ChatGPT)\n\n\
         \x20 McpHub remote status                    etat, profil, clients\n\
         \x20 McpHub remote enable --tunnel DOMAINE   tunnel Cloudflare (ex. hub.example.com)\n\
         \x20 McpHub remote enable --relay URL        relais (ex. https://relay.example.com)\n\
         \x20 McpHub remote disable                   coupe l'acces distant\n\
         \x20 McpHub remote allow SERVEUR             ajoute un serveur au profil remote\n\
         \x20 McpHub remote deny SERVEUR              retire un serveur du profil remote\n\
         \x20 McpHub remote allow-tool SERVEUR/OUTIL autorise un outil qui n'est pas reconnu en lecture\n\
         \x20 McpHub remote deny-tool SERVEUR/OUTIL  retire cet outil\n\
         \x20 McpHub remote clients                   clients connectes\n\
         \x20 McpHub remote revoke CLIENT_ID|all      deconnecte un client\n\
         \x20 McpHub remote approve CODE              valide une connexion (machine sans ecran)\n\n\
         Le profil remote ne montre que les serveurs nommes, et seulement leurs outils en lecture\n\
         seule stricte (readOnlyHint et nom de lecture) ou autorises nommement."
    );
}

fn print_status() -> i32 {
    let settings = match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    println!(
        "Acces distant : {}",
        if settings.enabled { "active" } else { "coupe" }
    );
    if !settings.public_url.is_empty() {
        println!("  Adresse MCP : {}/mcp", settings.public_url);
        println!("  Transport   : {}", settings.transport.as_str());
    }
    if settings.transport == Transport::Tunnel && settings.enabled {
        println!("  Socket      : {}", socket_path().display());
        println!("  Service     : {}", tunnel::service_state());
    }
    match crate::toolguard::remote_profile_problem() {
        Some(p) => println!("  Profil      : FERME, {}", p),
        None => println!(
            "  Profil      : lecture seule, serveurs {}",
            crate::toolguard::remote_profile_servers().join(", ")
        ),
    }
    match authz::list_clients() {
        Ok(clients) => println!("  Clients     : {}", clients.len()),
        Err(e) => println!("  Clients     : {}", e),
    }
    0
}

fn print_clients() -> i32 {
    match authz::list_clients() {
        Ok(clients) if clients.is_empty() => {
            println!("Aucun client.");
            0
        }
        Ok(clients) => {
            for c in clients {
                let state = if c.approved_at.is_some() {
                    "autorise"
                } else {
                    "jamais autorise"
                };
                let used = c
                    .last_used
                    .map(|t| crate::audit::iso_utc(t, 0))
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{}\n  nom : {}  type : {}  etat : {}  dernier jeton : {}\n  retour : {}",
                    c.client_id,
                    if c.name.is_empty() { "-" } else { &c.name },
                    c.kind,
                    state,
                    used,
                    c.redirect_uris.join(", ")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

async fn approve_request(code: &str, confirm: bool) -> Result<Value, String> {
    let url = format!(
        "http://127.0.0.1:{}/api/remote/approve",
        crate::instance::port()
    );
    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(crate::dashboard::get_auth_token())
        .json(&json!({"code": code, "confirm": confirm}))
        .send()
        .await
        .map_err(|e| {
            format!(
                "Demon McpHub injoignable ({}). Il doit tourner (McpHub install).",
                e
            )
        })?;
    if response.status().is_success() {
        Ok(response.json().await.unwrap_or_default())
    } else {
        Err(format!(
            "Refuse : {}",
            response.text().await.unwrap_or_default()
        ))
    }
}

/// Montre la demande (client, retour, origine) et ne valide qu'après un « o ».
async fn approve_cli(code: &str) -> i32 {
    let preview = match approve_request(code, false).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    let field = |k: &str| {
        preview
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    println!(
        "Client      : {}\nRetour vers : {}\nDepuis      : {}",
        field("client"),
        field("returnHost"),
        field("origin")
    );
    print!("Autoriser cette connexion ? [o/N] ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err()
        || !matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "o" | "oui" | "y" | "yes"
        )
    {
        println!("Rien n'a ete autorise.");
        return 1;
    }
    match approve_request(code, true).await {
        Ok(_) => {
            println!("Connexion validee : la page de connexion continue toute seule.");
            0
        }
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

/// Ajoute ou retire un outil autorisé nommément dans le profil `remote` ("serveur/outil").
pub fn set_profile_tool(key: &str, allowed: bool) -> Result<Vec<String>, String> {
    let valid = key.split_once('/').is_some_and(|(s, t)| {
        !s.is_empty() && !t.is_empty() && !key.contains('*') && !key.contains('?')
    });
    if !valid {
        return Err("format attendu : serveur/outil, sans joker".into());
    }
    let path = config_path();
    let mut config = crate::config::read_raw(&path)?.ok_or("config.json introuvable")?;
    let profile = config
        .pointer_mut("/settings/profiles/remote")
        .and_then(|p| p.as_object_mut())
        .ok_or("profil remote absent : McpHub remote allow SERVEUR d'abord")?;
    let mut tools: Vec<String> = profile
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    tools.retain(|t| t != key);
    if allowed {
        tools.push(key.to_string());
    }
    tools.sort();
    profile.insert("tools".into(), json!(tools));
    crate::config::write_raw_atomic(&path, &config)?;
    Ok(tools)
}

pub async fn run_cli(args: &[String]) -> i32 {
    // Hors du démon, la garde des outils n'a pas de config : on lui donne celle du fichier,
    // pour que status et allow décrivent le profil remote tel qu'il est écrit.
    if let Ok(Some(raw)) = crate::config::read_raw(&config_path()) {
        crate::toolguard::configure(crate::toolguard::parse_guard_config(&raw));
    }
    let arg = |i: usize| args.get(i).map(|s| s.as_str());
    match arg(0) {
        None | Some("status") => print_status(),
        Some("enable") => {
            let result = match (arg(1), arg(2)) {
                (Some("--tunnel"), Some(domain)) => tunnel::enable(domain),
                (Some("--relay"), Some(url)) => relay::enable(url),
                _ => {
                    print_usage();
                    return 2;
                }
            };
            match result {
                Ok(()) => {
                    tunnel::restart_daemon();
                    print_status();
                    if crate::toolguard::remote_profile_problem().is_some() {
                        println!("\nAjoutez au moins un serveur : McpHub remote allow SERVEUR");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        Some("disable") => {
            let result = Settings::load().and_then(|mut s| {
                if s.transport == Transport::Tunnel {
                    tunnel::remove_service()?;
                }
                s.enabled = false;
                save_settings(&s)
            });
            match result {
                Ok(()) => {
                    tunnel::restart_daemon();
                    println!("Acces distant coupe. Les clients gardent leur inscription : McpHub remote revoke all pour les retirer.");
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        Some(cmd @ ("allow" | "deny")) => {
            let Some(server) = arg(1) else {
                print_usage();
                return 2;
            };
            match set_profile_server(server, cmd == "allow") {
                Ok(servers) => {
                    println!(
                        "Profil remote (lecture seule) : {}",
                        if servers.is_empty() {
                            "aucun serveur, acces distant ferme".to_string()
                        } else {
                            servers.join(", ")
                        }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        Some(cmd @ ("allow-tool" | "deny-tool")) => {
            let Some(key) = arg(1) else {
                print_usage();
                return 2;
            };
            match set_profile_tool(key, cmd == "allow-tool") {
                Ok(tools) => {
                    println!(
                        "Outils autorises nommement : {}",
                        if tools.is_empty() {
                            "aucun".to_string()
                        } else {
                            tools.join(", ")
                        }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        Some("clients") => print_clients(),
        Some("revoke") => match arg(1) {
            Some(target) => match authz::revoke_client(target) {
                Ok(0) => {
                    eprintln!("Client inconnu : {}", target);
                    1
                }
                Ok(n) => {
                    println!("{} client(s) deconnecte(s).", n);
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            },
            None => {
                print_usage();
                2
            }
        },
        Some("approve") => match arg(1) {
            Some(code) => approve_cli(code).await,
            None => {
                print_usage();
                2
            }
        },
        _ => {
            print_usage();
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_validate() {
        let s = Settings::from_json(Some(&json!({
            "enabled": true,
            "transport": "relay",
            "publicUrl": "https://Abc.Relay.Example.com/",
        })));
        assert!(s.enabled);
        assert_eq!(s.transport, Transport::Relay);
        assert_eq!(s.public_url, "https://Abc.Relay.Example.com");
        assert_eq!(s.public_host().as_deref(), Some("abc.relay.example.com"));

        let off = Settings::from_json(None);
        assert!(!off.enabled);
        assert_eq!(off.public_host(), None);

        for bad in [
            "http://hub.example.com",
            "https://hub.example.com/sub",
            "nope",
        ] {
            let s = Settings::from_json(Some(&json!({"publicUrl": bad})));
            assert_eq!(s.public_host(), None, "{}", bad);
        }
    }
}
