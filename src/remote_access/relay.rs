//! Transport « relais » : le démon ouvre un WebSocket sortant vers un relais Cloudflare
//! (dossier `relay/` du dépôt), qui lui transmet les requêtes reçues sur
//! `https://<hubId>.<domaine du relais>`. Aucun compte Cloudflare ni domaine côté utilisateur.
//!
//! Le relais ne fait que transporter : l'OAuth, le consentement, le profil `remote` et le
//! journal d'audit restent dans McpHub. L'identifiant du hub (aléatoire, 128 bits) et son
//! secret sont dans `~/.McpHub/remote/relay.json` (mode 600, caché des serveurs isolés) ; le
//! relais ne garde que l'empreinte du secret, enregistrée à la première connexion.
//!
//! Trame binaire, dans les deux sens : 4 octets (longueur de l'en-tête JSON, gros-boutiste),
//! l'en-tête JSON, puis le corps brut.
//! - requête : `{"t":"req","id","method","path","query","headers":{...}}`
//! - réponse : `{"t":"res","id","status","headers":[[nom, valeur], ...]}`

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use super::http::{split_response, Request, Response, MAX_BODY};
use super::{save_settings, Settings, Transport};
use crate::proxy::ProxyServer;

const MAX_CONCURRENT: usize = 32;
const PING_EVERY: Duration = Duration::from_secs(30);
const SILENCE_LIMIT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    #[serde(rename = "hubId")]
    pub hub_id: String,
    pub secret: String,
}

fn identity_path() -> std::path::PathBuf {
    super::remote_dir().join("relay.json")
}

pub fn load_identity() -> Result<Identity, String> {
    let path = identity_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{} : {} (McpHub remote enable --relay URL)",
            path.display(),
            e
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("{} illisible : {}", path.display(), e))
}

fn save_identity(identity: &Identity) -> Result<(), String> {
    super::ensure_private_dir()?;
    let path = identity_path();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(identity).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("{} : {}", tmp.display(), e))?;
    crate::vault::restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| format!("{} : {}", path.display(), e))
}

/// Identifiant de hub dérivé du secret : les 26 premiers caractères base32 (minuscules) de
/// son SHA-256. Le relais refait le calcul à chaque connexion : personne ne peut revendiquer
/// un identifiant sans en connaître le secret, et le relais n'a rien à garder.
pub fn hub_id_for(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    base32(&digest[..17])[..26].to_string()
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bits: u32 = 0;
    let mut acc: u32 = 0;
    let mut out = String::new();
    for b in bytes {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn relay_host(url: &str) -> Result<String, String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| format!("adresse du relais invalide : {}", url))?;
    if parsed.scheme() != "https" || parsed.path() != "/" || parsed.query().is_some() {
        return Err("adresse du relais : https://domaine, sans chemin".into());
    }
    parsed
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| "adresse du relais sans hote".into())
}

pub fn enable(url: &str) -> Result<(), String> {
    let url = url.trim().trim_end_matches('/');
    let host = relay_host(url)?;
    let identity = match load_identity() {
        Ok(existing) if existing.hub_id == hub_id_for(&existing.secret) => existing,
        _ => {
            let secret = format!(
                "mhrelay_{}",
                crate::oauth::b64url(&super::authz::random_bytes(32)?)
            );
            let identity = Identity {
                hub_id: hub_id_for(&secret),
                secret,
            };
            save_identity(&identity)?;
            identity
        }
    };
    // Le tunnel éventuel n'a plus d'usage : son service est retiré.
    super::tunnel::remove_service()?;
    save_settings(&Settings {
        enabled: true,
        transport: Transport::Relay,
        public_url: format!("https://{}.{}", identity.hub_id, host),
        relay_url: Some(url.to_string()),
    })
}

fn connect_url(settings: &Settings, identity: &Identity) -> Result<String, String> {
    let relay = settings
        .relay_url
        .as_deref()
        .ok_or("relayUrl absent des reglages")?;
    let host = relay_host(relay)?;
    Ok(format!("wss://{}/connect?hub={}", host, identity.hub_id))
}

pub fn encode_frame(header: &Value, body: &[u8]) -> Vec<u8> {
    let head = header.to_string().into_bytes();
    let mut out = Vec::with_capacity(4 + head.len() + body.len());
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(body);
    out
}

pub fn decode_frame(frame: &[u8]) -> Option<(Value, Vec<u8>)> {
    let len = u32::from_be_bytes(frame.get(..4)?.try_into().ok()?) as usize;
    let head = frame.get(4..4 + len)?;
    let header: Value = serde_json::from_slice(head).ok()?;
    Some((header, frame[4 + len..].to_vec()))
}

fn request_from(header: &Value, body: Vec<u8>) -> Option<(String, Request)> {
    if header.get("t").and_then(|v| v.as_str()) != Some("req") {
        return None;
    }
    let id = header.get("id")?.as_str()?.to_string();
    let headers = header
        .get("headers")
        .and_then(|h| h.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.to_ascii_lowercase(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some((
        id,
        Request {
            method: header.get("method")?.as_str()?.to_string(),
            path: header.get("path")?.as_str()?.to_string(),
            query: header
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .trim_start_matches('?')
                .to_string(),
            headers,
            body,
        },
    ))
}

fn response_frame(id: &str, raw: &[u8]) -> Vec<u8> {
    let Response {
        status,
        headers,
        body,
    } = split_response(raw).unwrap_or_else(|| Response::text(502, "bad response from McpHub"));
    let headers: Vec<Value> = headers
        .into_iter()
        .filter(|(k, _)| {
            !matches!(
                k.to_ascii_lowercase().as_str(),
                "connection" | "content-length" | "transfer-encoding"
            )
        })
        .map(|(k, v)| json!([k, v]))
        .collect();
    encode_frame(
        &json!({"t": "res", "id": id, "status": status, "headers": headers}),
        &body,
    )
}

/// Boucle du démon : connexion, traitement des requêtes, reconnexion avec attente croissante.
pub async fn run(proxy: Arc<ProxyServer>, settings: Arc<Settings>) {
    let identity = match load_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[McpHub][REMOTE] Relais : {}", e);
            return;
        }
    };
    let url = match connect_url(&settings, &identity) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[McpHub][REMOTE] Relais : {}", e);
            return;
        }
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&proxy, &settings, &identity, &url).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => eprintln!("[McpHub][REMOTE] Relais deconnecte : {}", e),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn session(
    proxy: &Arc<ProxyServer>,
    settings: &Arc<Settings>,
    identity: &Identity,
    url: &str,
) -> Result<(), String> {
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    let auth =
        HeaderValue::from_str(&format!("Bearer {}", identity.secret)).map_err(|e| e.to_string())?;
    request.headers_mut().insert("Authorization", auth);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!(
        "[McpHub][REMOTE] Relais connecte : {}/mcp",
        settings.public_url
    );
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut last_seen = tokio::time::Instant::now();
    let outcome = loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_seen.elapsed() > SILENCE_LIMIT {
                    break Err("relais silencieux depuis 90 s".to_string());
                }
                if tx.send(Message::Ping(Vec::new().into())).is_err() {
                    break Err("envoi impossible".to_string());
                }
            }
            incoming = stream.next() => {
                let Some(incoming) = incoming else { break Ok(()) };
                let message = match incoming {
                    Ok(m) => m,
                    Err(e) => break Err(e.to_string()),
                };
                last_seen = tokio::time::Instant::now();
                match message {
                    Message::Binary(frame) => {
                        // Trame illisible : impossible de répondre sans identifiant, le relais
                        // renverra son 504. Trame trop grosse : refus immédiat.
                        let Some((header, body)) = decode_frame(&frame) else { continue };
                        if frame.len() > MAX_BODY + 64 * 1024 {
                            if let Some(id) = header.get("id").and_then(|v| v.as_str()) {
                                let refused = Response::text(413, "payload too large").to_bytes();
                                let _ = tx.send(Message::Binary(response_frame(id, &refused).into()));
                            }
                            continue;
                        }
                        let Some((id, req)) = request_from(&header, body) else { continue };
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            let busy = Response::text(503, "McpHub busy").to_bytes();
                            let _ = tx.send(Message::Binary(response_frame(&id, &busy).into()));
                            continue;
                        };
                        let proxy = proxy.clone();
                        let settings = settings.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let raw = super::handle(&proxy, &settings, req).await;
                            let _ = tx.send(Message::Binary(response_frame(&id, &raw).into()));
                            drop(permit);
                        });
                    }
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
        }
    };
    writer.abort();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_id_is_a_dns_label_derived_from_the_secret() {
        let id = hub_id_for("mhrelay_secret");
        assert_eq!(id.len(), 26);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
        assert_eq!(id, hub_id_for("mhrelay_secret"));
        assert_ne!(id, hub_id_for("mhrelay_other"));
        // Même calcul que le relais (relay/src/index.ts, hubIdFor).
        assert_eq!(hub_id_for("abc"), "xj4bnp4pahh6uqkbidpf3lrceo");
    }

    #[test]
    fn frames_round_trip() {
        let frame = encode_frame(
            &json!({"t":"req","id":"1","method":"POST","path":"/mcp","query":"?a=1","headers":{"Host":"x.relay.example.com"}}),
            b"{}",
        );
        let (header, body) = decode_frame(&frame).unwrap();
        let (id, req) = request_from(&header, body).unwrap();
        assert_eq!(id, "1");
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.query, "a=1");
        assert_eq!(req.header("host"), Some("x.relay.example.com"));
        assert_eq!(req.body, b"{}");
        assert!(decode_frame(&[0, 0, 0, 9, b'{']).is_none());
    }

    #[test]
    fn response_frames_drop_hop_headers() {
        let raw = Response::json(200, &json!({"ok": 1})).to_bytes();
        let (header, body) = decode_frame(&response_frame("7", &raw)).unwrap();
        assert_eq!(header["status"], 200);
        assert_eq!(header["id"], "7");
        let names: Vec<String> = header["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_ascii_lowercase())
            .collect();
        assert!(!names.contains(&"connection".to_string()));
        assert!(!names.contains(&"content-length".to_string()));
        assert_eq!(body, br#"{"ok":1}"#);
    }

    #[test]
    fn relay_url_rules() {
        assert_eq!(
            relay_host("https://Relay.Example.com").unwrap(),
            "relay.example.com"
        );
        assert!(relay_host("http://relay.example.com").is_err());
        assert!(relay_host("https://relay.example.com/x").is_err());
    }
}
