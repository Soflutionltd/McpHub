//! Client MCP "Streamable HTTP" pour les serveurs distants (sans mcp-remote).
//!
//! - POST JSON-RPC, réponse en JSON ou en flux SSE (on lit jusqu'à la réponse attendue) ;
//! - en-têtes `Mcp-Session-Id` (si le serveur en donne un) et `MCP-Protocol-Version` ;
//! - authentification : en-têtes fixes de la config (`headers`, avec ${VAR}) et/ou OAuth
//!   (jeton du Trousseau, rafraîchi tout seul ; sur 401, un rafraîchissement puis une erreur
//!   claire « McpHub oauth login SERVEUR » plutôt qu'un navigateur ouvert sans prévenir).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::sync::Mutex;

pub struct RemoteClient {
    server: String,
    url: String,
    headers: Vec<(String, String)>,
    oauth: bool,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl RemoteClient {
    pub fn new(server: &str, url: &str, headers: &HashMap<String, String>, oauth: bool) -> Self {
        let mut hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        hs.sort();
        Self {
            server: server.to_string(),
            url: url.to_string(),
            headers: hs,
            oauth,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .user_agent(concat!("McpHub/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    async fn post(&self, body: &Value, token: Option<&str>) -> Result<reqwest::Response, String> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        if let Some(s) = self.session.lock().await.clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        if let Some(p) = self.protocol.lock().await.clone() {
            req = req.header("MCP-Protocol-Version", p);
        }
        req.json(body)
            .send()
            .await
            .map_err(|e| format!("Read error: {} injoignable : {}", self.server, e))
    }

    /// Envoie avec authentification ; sur 401, rafraîchit une fois puis abandonne proprement.
    async fn send(&self, body: &Value) -> Result<reqwest::Response, String> {
        let mut token = if self.oauth {
            crate::oauth::access_token(&self.server, &self.url).await?
        } else {
            None
        };
        let mut retried = false;
        loop {
            let resp = self.post(body, token.as_deref()).await?;
            if resp.status().as_u16() == 401 && self.oauth && !retried {
                retried = true;
                if let Ok(Some(t)) = crate::oauth::force_refresh(&self.server, &self.url).await {
                    token = Some(t);
                    continue;
                }
                return Err(format!(
                    "{} : connexion requise. Lancer : McpHub oauth login {}",
                    self.server, self.server
                ));
            }
            if resp.status().as_u16() == 401 {
                return Err(format!(
                    "{} : authentification refusee (401). {}",
                    self.server,
                    if self.oauth {
                        format!("Lancer : McpHub oauth login {}", self.server)
                    } else {
                        "Verifier le jeton dans headers.".to_string()
                    }
                ));
            }
            if resp.status().as_u16() == 404 && self.session.lock().await.is_some() {
                *self.session.lock().await = None;
                return Err(format!("Server closed connection: session {} expiree", self.server));
            }
            return Ok(resp);
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.send(&body).await?;
        if method == "initialize" {
            if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                *self.session.lock().await = Some(s.to_string());
            }
        }
        let status = resp.status();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "MCP error: HTTP {} de {} : {}",
                status,
                self.server,
                crate::redaction::redact_text(&text.chars().take(300).collect::<String>())
            ));
        }
        let message = if ctype.starts_with("text/event-stream") {
            read_sse_response(resp, id).await?
        } else {
            resp.json::<Value>()
                .await
                .map_err(|e| format!("Read error: reponse illisible de {} : {}", self.server, e))?
        };
        let message = match message {
            Value::Array(items) => items
                .into_iter()
                .find(|m| m.get("id").and_then(|v| v.as_u64()) == Some(id))
                .unwrap_or(Value::Null),
            m => m,
        };
        if let Some(err) = message.get("error") {
            return Err(format!("MCP error: {}", err));
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        if method == "initialize" {
            if let Some(v) = result.get("protocolVersion").and_then(|v| v.as_str()) {
                *self.protocol.lock().await = Some(v.to_string());
            }
        }
        Ok(result)
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        let resp = self.send(&body).await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("MCP error: notification refusee (HTTP {})", resp.status()))
        }
    }

    /// Fin de session côté serveur (DELETE), au mieux.
    pub async fn close(&self) {
        let Some(s) = self.session.lock().await.take() else { return };
        let mut req = self.http.delete(&self.url).header("Mcp-Session-Id", s);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), req.send()).await;
    }
}

/// Taille maximale d'une réponse (flux SSE) : 64 Mo.
const MAX_SSE_BYTES: usize = 64 * 1024 * 1024;

/// Lit un flux SSE jusqu'au message JSON-RPC portant l'identifiant attendu.
async fn read_sse_response(mut resp: reqwest::Response, id: u64) -> Result<Value, String> {
    // Octets bruts : un caractère UTF-8 peut être coupé entre deux morceaux réseau, on ne
    // décode donc que des lignes complètes.
    let mut buf: Vec<u8> = Vec::new();
    let mut data = String::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| format!("Read error: flux SSE : {}", e))?;
        let Some(chunk) = chunk else {
            return Err("Server closed connection: flux SSE termine sans reponse".into());
        };
        buf.extend_from_slice(&chunk);
        if buf.len() + data.len() > MAX_SSE_BYTES {
            return Err("Read error: reponse SSE trop volumineuse".into());
        }
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            if line.is_empty() {
                if !data.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if v.get("id").and_then(|x| x.as_u64()) == Some(id)
                            && (v.get("result").is_some() || v.get("error").is_some())
                        {
                            return Ok(v);
                        }
                    }
                    data.clear();
                }
                continue;
            }
            if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.strip_prefix(' ').unwrap_or(d));
            }
        }
    }
}
