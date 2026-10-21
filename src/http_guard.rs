//! Protection du serveur HTTP local (port 24680).
//!
//! Pourquoi : le serveur écoute sur 127.0.0.1, mais n'importe quelle page web ouverte dans le
//! navigateur peut envoyer des requêtes vers 127.0.0.1 (requêtes "simples" sans pré-vol CORS) ou
//! s'y attaquer par DNS rebinding (un domaine qui se met à pointer vers 127.0.0.1). Sans contrôle,
//! une page malveillante pouvait lire la config et ajouter un serveur MCP, donc exécuter une
//! commande sur la machine.
//!
//! Règles appliquées à toutes les requêtes :
//! 1. En-tête Host : uniquement 127.0.0.1, localhost ou [::1] (bloque le DNS rebinding).
//! 2. En-tête Origin, s'il est présent : uniquement l'origine du tableau de bord lui-même.
//! 3. Routes /api : jeton Bearer, ou cookie de session posé par le tableau de bord
//!    (HttpOnly, SameSite=Strict, donc jamais envoyé par un site tiers).
//! 4. Plus aucun `Access-Control-Allow-Origin: *`.

use std::collections::HashMap;

pub const SESSION_COOKIE: &str = "mcphub_session";

const LOCAL_HOSTS: &[&str] = &["127.0.0.1", "localhost", "[::1]"];

/// Sépare "hote:port" en (hote, port optionnel), en gérant les crochets IPv6.
fn split_host_port(value: &str) -> (String, Option<u16>) {
    let v = value.trim().to_ascii_lowercase();
    if let Some(rest) = v.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = format!("[{}]", &rest[..end]);
            let after = &rest[end + 1..];
            let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
            return (host, port);
        }
        return (v, None);
    }
    match v.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h.to_string(), Some(port)),
            Err(_) => (v.clone(), Some(0)),
        },
        _ => (v, None),
    }
}

/// Host doit désigner la machine locale (et le bon port s'il est indiqué).
/// Un Host absent est toléré (clients HTTP/1.0 locaux), un Host étranger est refusé.
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return true };
    let (h, p) = split_host_port(host);
    LOCAL_HOSTS.contains(&h.as_str()) && p.map_or(true, |p| p == port)
}

/// Origin, s'il est envoyé (navigateurs), doit être le tableau de bord local.
/// "null" (iframe sandbox, fichier local) est refusé.
pub fn origin_allowed(origin: Option<&str>, port: u16) -> bool {
    let Some(origin) = origin else { return true };
    let o = origin.trim().to_ascii_lowercase();
    let Some(rest) = o.strip_prefix("http://") else {
        return false;
    };
    let rest = rest.trim_end_matches('/');
    if rest.contains('/') {
        return false;
    }
    let (h, p) = split_host_port(rest);
    LOCAL_HOSTS.contains(&h.as_str()) && p == Some(port)
}

/// Comparaison en temps constant (pas de fuite par la durée de comparaison).
pub fn token_matches(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

pub fn bearer_token(headers: &HashMap<String, String>) -> Option<&str> {
    let v = headers.get("authorization")?;
    let v = v.trim();
    if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
        Some(v[7..].trim())
    } else {
        None
    }
}

pub fn cookie_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    let cookies = headers.get("cookie")?;
    cookies.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.trim() == name {
            Some(v.trim())
        } else {
            None
        }
    })
}

/// Jeton Bearer valide, ou cookie de session valide.
pub fn is_authorized(headers: &HashMap<String, String>, expected: &str) -> bool {
    if let Some(t) = bearer_token(headers) {
        if token_matches(t, expected) {
            return true;
        }
    }
    if let Some(t) = cookie_value(headers, SESSION_COOKIE) {
        if token_matches(t, expected) {
            return true;
        }
    }
    false
}

/// En-tête Set-Cookie de la session du tableau de bord.
pub fn session_cookie_header(token: &str) -> String {
    format!(
        "Set-Cookie: {}={}; HttpOnly; SameSite=Strict; Path=/\r\n",
        SESSION_COOKIE, token
    )
}

/// 32 octets aléatoires du système, en hexadécimal.
pub fn random_token() -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(format!(
        "mcphub_{}",
        buf.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn host_local_ok_and_rebinding_refused() {
        assert!(host_allowed(Some("127.0.0.1:24680"), 24680));
        assert!(host_allowed(Some("localhost:24680"), 24680));
        assert!(host_allowed(Some("LOCALHOST"), 24680));
        assert!(host_allowed(Some("[::1]:24680"), 24680));
        assert!(host_allowed(None, 24680));
        assert!(!host_allowed(Some("evil.example:24680"), 24680));
        assert!(!host_allowed(Some("127.0.0.1.evil.example"), 24680));
        assert!(!host_allowed(Some("127.0.0.1:9999"), 24680));
        assert!(!host_allowed(Some("127.0.0.1:abc"), 24680));
    }

    #[test]
    fn origin_rules() {
        assert!(origin_allowed(None, 24680));
        assert!(origin_allowed(Some("http://127.0.0.1:24680"), 24680));
        assert!(origin_allowed(Some("http://localhost:24680"), 24680));
        assert!(!origin_allowed(Some("null"), 24680));
        assert!(!origin_allowed(Some("https://evil.example"), 24680));
        assert!(!origin_allowed(Some("http://127.0.0.1"), 24680));
        assert!(!origin_allowed(Some("http://127.0.0.1:24680.evil.example"), 24680));
        assert!(!origin_allowed(Some("http://localhost:3000"), 24680));
    }

    #[test]
    fn auth_by_bearer_or_cookie() {
        let tok = "mcphub_abc";
        assert!(is_authorized(&h(&[("authorization", "Bearer mcphub_abc")]), tok));
        assert!(is_authorized(&h(&[("authorization", "bearer mcphub_abc")]), tok));
        assert!(is_authorized(
            &h(&[("cookie", "a=1; mcphub_session=mcphub_abc; b=2")]),
            tok
        ));
        assert!(!is_authorized(&h(&[("authorization", "Bearer nope")]), tok));
        assert!(!is_authorized(&h(&[("cookie", "mcphub_session=")]), tok));
        assert!(!is_authorized(&h(&[]), tok));
        assert!(!token_matches("mcphub_ab", tok));
        assert!(!token_matches("", ""));
    }

    #[test]
    fn random_token_is_long_and_unique() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 7 + 64);
        assert_ne!(a, b);
    }
}
