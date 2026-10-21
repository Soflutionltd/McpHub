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
//! 3. Routes /api : jeton Bearer (le jeton principal, ou le secret de session du tableau de
//!    bord). Pas de cookie : un cookie localhost est envoyé à tous les ports locaux.
//!    Le tableau de bord obtient son secret par un lien à usage unique (`McpHub dashboard`)
//!    et le garde dans sessionStorage, propre à l'origine (port compris).
//! 4. Plus aucun `Access-Control-Allow-Origin: *` : un site tiers ne peut pas ajouter
//!    l'en-tête Authorization (pré-vol CORS refusé), donc pas de CSRF.

use std::collections::HashMap;


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

/// Comparaison en temps constant (longueurs différentes = refus immédiat).
pub fn token_matches(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
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

/// Secret de session du tableau de bord : aléatoire, propre à ce processus, distinct du
/// jeton principal et valable seulement pour /api.
pub fn session_secret() -> &'static str {
    static SECRET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SECRET.get_or_init(|| {
        random_token()
            .map(|t| t.replacen("mcphub_", "dash_", 1))
            .unwrap_or_else(|| format!("dash_{:x}", std::process::id() as u128 * 0x9E37_79B9_7F4A_7C15))
    })
}

/// Jeton principal, ou secret de session (Bearer, ou `session=` pour EventSource qui ne
/// sait pas envoyer d'en-tête).
pub fn api_authorized(headers: &HashMap<String, String>, query_session: Option<&str>, master: &str) -> bool {
    if let Some(t) = bearer_token(headers) {
        if token_matches(t, master) || token_matches(t, session_secret()) {
            return true;
        }
    }
    query_session.map_or(false, |t| token_matches(t, session_secret()))
}

/// Jeton au format actuel (32 octets aléatoires) ; sinon il est régénéré.
pub fn token_is_current(t: &str) -> bool {
    t.len() == 7 + 64
        && t.starts_with("mcphub_")
        && t[7..].bytes().all(|c| c.is_ascii_hexdigit())
}

fn login_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".McpHub")
        .join("dashboard-login")
}

/// Lien de connexion à usage unique (valable 2 minutes), écrit dans un fichier 600 que seul
/// l'utilisateur peut lire : seul un processus de l'utilisateur peut ouvrir le tableau de bord.
pub fn create_login_nonce() -> Option<String> {
    use std::io::Write;
    let nonce = random_token()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let path = login_path();
    let _ = std::fs::create_dir_all(path.parent()?);
    let _ = std::fs::remove_file(&path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).ok()?;
    f.write_all(format!("{} {}", nonce, now).as_bytes()).ok()?;
    Some(nonce)
}

pub fn consume_login_nonce(candidate: &str) -> bool {
    let path = login_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let mut parts = content.split_whitespace();
    let (Some(nonce), Some(ts)) = (parts.next(), parts.next()) else {
        return false;
    };
    let ts: u64 = ts.parse().unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !token_matches(candidate, nonce) {
        return false;
    }
    let _ = std::fs::remove_file(&path);
    now.saturating_sub(ts) <= 120
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
    fn api_auth_bearer_ou_session() {
        let tok = "mcphub_abc";
        assert!(api_authorized(&h(&[("authorization", "Bearer mcphub_abc")]), None, tok));
        assert!(api_authorized(&h(&[("authorization", "bearer mcphub_abc")]), None, tok));
        let bearer = format!("Bearer {}", session_secret());
        assert!(api_authorized(&h(&[("authorization", bearer.as_str())]), None, tok));
        assert!(api_authorized(&h(&[]), Some(session_secret()), tok));
        assert!(!api_authorized(&h(&[]), Some(tok), tok), "le jeton principal ne passe pas en URL");
        assert!(!api_authorized(&h(&[("authorization", "Bearer nope")]), None, tok));
        assert!(!api_authorized(&h(&[("cookie", "mcphub_session=mcphub_abc")]), None, tok));
        assert!(!token_matches("mcphub_ab", tok));
        let mut padded = tok.to_string();
        padded.push_str(&"\0".repeat(256));
        assert!(!token_matches(&padded, tok));
        assert!(!token_matches("", ""));
        assert!(token_is_current(&random_token().unwrap()));
        assert!(!token_is_current("mcphub_18d2f3a0b"));
    }

    #[test]
    fn random_token_is_long_and_unique() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 7 + 64);
        assert_ne!(a, b);
    }
}
