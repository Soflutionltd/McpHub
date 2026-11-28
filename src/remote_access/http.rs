//! Requêtes et réponses HTTP de l'accès distant, communes au tunnel et au relais.
//!
//! Le tunnel livre des octets HTTP bruts sur le port local dédié ; le relais livre des
//! requêtes déjà découpées (méthode, chemin, en-têtes, corps). Les deux aboutissent au même
//! `Request`, et la réponse repart en octets HTTP (`Connection: close`, `Content-Length`).

use std::collections::HashMap;

/// Corps refusé au-delà de 16 Mo, comme sur le port local principal.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    /// Chemin sans la chaîne de requête.
    pub path: String,
    /// Chaîne de requête, sans le `?`.
    pub query: String,
    /// Noms en minuscules.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.as_str())
    }

    pub fn param(&self, key: &str) -> Option<String> {
        parse_urlencoded(&self.query).remove(key)
    }

    pub fn params(&self) -> HashMap<String, String> {
        parse_urlencoded(&self.query)
    }

    /// Corps `application/x-www-form-urlencoded`.
    pub fn form(&self) -> HashMap<String, String> {
        parse_urlencoded(&String::from_utf8_lossy(&self.body))
    }

    /// Adresse du client telle que Cloudflare la transmet (tunnel et relais passent tous deux
    /// par Cloudflare). Le port local n'écoute que sur 127.0.0.1 : rien d'autre ne l'atteint
    /// depuis Internet.
    pub fn client_ip(&self) -> String {
        self.header("cf-connecting-ip")
            .map(|ip| ip.trim().to_string())
            .filter(|ip| !ip.is_empty())
            .unwrap_or_else(|| "local".to_string())
    }

    /// Hôte demandé, sans le port, en minuscules.
    pub fn host(&self) -> Option<String> {
        let raw = self.header("host")?.trim().to_ascii_lowercase();
        let host = match raw.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
            _ => raw,
        };
        Some(host)
    }

    pub fn prefers_french(&self) -> bool {
        self.header("accept-language")
            .and_then(|value| value.split(',').next())
            .map(|tag| tag.trim().to_ascii_lowercase())
            .is_some_and(|tag| tag == "fr" || tag.starts_with("fr-") || tag.starts_with("fr;"))
    }
}

/// En-têtes d'une requête brute. Retourne la requête sans corps, la longueur des en-têtes et
/// la longueur annoncée du corps.
pub fn parse_head(raw: &[u8]) -> Option<(Request, usize, usize)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = match headers.get("content-length") {
        Some(value) => value.trim().parse().ok()?,
        None => 0,
    };
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    Some((
        Request {
            method,
            path,
            query,
            headers,
            body: Vec::new(),
        },
        end,
        content_length,
    ))
}

pub fn urldecode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(raw[i + 1]), hex(raw[i + 2])) {
                    (Some(high), Some(low)) => {
                        bytes.push((high * 16 + low) as u8);
                        i += 3;
                    }
                    _ => {
                        bytes.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            other => {
                bytes.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

pub fn parse_urlencoded(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        // Premier gagnant : un paramètre répété ne peut pas remplacer le premier.
        map.entry(urldecode(key))
            .or_insert_with(|| urldecode(value));
    }
    map
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body,
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        Response::new(status, "application/json", value.to_string().into_bytes())
    }

    /// Erreur OAuth (RFC 6749 §5.2).
    pub fn oauth_error(status: u16, code: &str, description: &str) -> Response {
        Response::json(
            status,
            &serde_json::json!({"error": code, "error_description": description}),
        )
    }

    pub fn html(status: u16, body: String) -> Response {
        Response::new(status, "text/html; charset=utf-8", body.into_bytes()).with(
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'",
        )
    }

    pub fn text(status: u16, body: &str) -> Response {
        Response::new(
            status,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    pub fn redirect(location: &str) -> Response {
        Response {
            status: 302,
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    pub fn with(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            head.push_str(&format!("{}: {}\r\n", name, value));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
            self.body.len()
        ));
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Découpe une réponse HTTP complète (statut, en-têtes, corps) pour le relais.
pub fn split_response(raw: &[u8]) -> Option<Response> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    Some(Response {
        status,
        headers,
        body: raw[end..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_head_and_query() {
        let raw = b"POST /token?x=1&x=2 HTTP/1.1\r\nHost: Hub.Example.com:443\r\nContent-Length: 5\r\n\r\nhello";
        let (req, end, len) = parse_head(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/token");
        assert_eq!(req.param("x").as_deref(), Some("1"));
        assert_eq!(req.host().as_deref(), Some("hub.example.com"));
        assert_eq!(len, 5);
        assert_eq!(&raw[end..], b"hello");
    }

    #[test]
    fn rejects_invalid_content_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n";
        assert!(parse_head(raw).is_none());
    }

    #[test]
    fn url_round_trip() {
        let original = "https://a.b/c?d=e f&g=é";
        assert_eq!(urldecode(&urlencode(original)), original);
        assert_eq!(urldecode("%zz%41+"), "%zzA ");
        assert_eq!(urldecode("%4"), "%4");
    }

    #[test]
    fn response_round_trip() {
        let bytes = Response::json(201, &serde_json::json!({"ok": true})).to_bytes();
        let parsed = split_response(&bytes).unwrap();
        assert_eq!(parsed.status, 201);
        assert!(parsed
            .headers
            .iter()
            .any(|(k, v)| k == "Cache-Control" && v == "no-store"));
        assert_eq!(parsed.body, br#"{"ok":true}"#);
    }
}
