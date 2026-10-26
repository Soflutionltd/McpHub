//! Bibliothèque : le registre MCP officiel (registry.modelcontextprotocol.io) branché sur McpHub.
//!
//! Poids nul pour l'agent : le catalogue n'est lu qu'à la demande (tableau de bord, commande
//! `McpHub registry`, ou `mcphub_call` avec `server: "mcphub"`), mis en cache sur disque
//! (~/.McpHub/registry-cache) et n'ajoute aucune définition d'outil au contexte.
//!
//! Chaque fiche reçoit un score de confiance avant installation : éditeur vérifié, popularité et
//! activité du dépôt, fraîcheur, analyse statique de ce que le serveur demande (secrets, accès,
//! options Docker dangereuses, intégrité du paquet). Une installation hérite des protections de
//! McpHub : bac à sable (bouclier), épinglage des outils au premier contact et quarantaine en cas
//! de changement, journal d'audit, secrets au Trousseau, OAuth natif pour les serveurs distants.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const API: &str = "https://registry.modelcontextprotocol.io/v0.1/servers";
const OFFICIAL: &str = "io.modelcontextprotocol.registry/official";
const PUBLISHER: &str = "io.modelcontextprotocol.registry/publisher-provided";
/// Registre MCP de GitHub : sous-registre au même format, limité aux serveurs populaires, avec les
/// étoiles et l'activité de chaque dépôt, et rapide. Sert de première page de la bibliothèque.
const GITHUB_API: &str = "https://api.mcp.github.com/v0.1/servers";
const LIST_TTL_SECS: u64 = 6 * 3600;
const ENTRY_TTL_SECS: u64 = 6 * 3600;
const REPO_TTL_SECS: u64 = 24 * 3600;
const MAX_BUNDLE_BYTES: usize = 300 * 1024 * 1024;

// ─── Cache disque ────────────────────────────────────────────

fn cache_dir() -> PathBuf {
    crate::instance::hub_dir().join("registry-cache")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short_hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn cache_file(kind: &str, key: &str) -> PathBuf {
    cache_dir().join(format!("{}-{}.json", kind, short_hash(key)))
}

/// Valeur en cache et vrai si elle est encore fraîche.
fn cache_get(path: &Path, ttl: u64) -> Option<(Value, bool)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let at = v.get("at")?.as_u64()?;
    Some((v.get("data")?.clone(), now_secs().saturating_sub(at) < ttl))
}

fn cache_put(path: &Path, data: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, json!({"at": now_secs(), "data": data}).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("McpHub/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn fetch_json(url: &str, headers: &[(&str, String)]) -> Result<Value, String> {
    let mut req = http().get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.map_err(|e| format!("reseau : {}", e))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Err("introuvable dans le registre".into());
    }
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| format!("reponse illisible : {}", e))
}

/// Lecture avec cache : copie fraîche sur disque, sinon réseau, sinon (hors ligne) copie périmée.
/// Renvoie aussi vrai quand la copie servie est périmée.
async fn cached_json(
    kind: &str,
    url: &str,
    ttl: u64,
    headers: &[(&str, String)],
) -> Result<(Value, bool), String> {
    let path = cache_file(kind, url);
    let cached = cache_get(&path, ttl);
    if let Some((v, true)) = &cached {
        return Ok((v.clone(), false));
    }
    match fetch_json(url, headers).await {
        Ok(v) => {
            cache_put(&path, &v);
            Ok((v, false))
        }
        Err(e) => match cached {
            Some((v, _)) => Ok((v, true)),
            None => Err(e),
        },
    }
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ─── Lecture du registre ─────────────────────────────────────

fn meta<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
    entry.get("_meta").and_then(|m| m.get(OFFICIAL)).and_then(|o| o.get(key))
}

fn status(entry: &Value) -> &str {
    meta(entry, "status").and_then(|v| v.as_str()).unwrap_or("active")
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

// ─── Catalogue local ─────────────────────────────────────────
//
// L'API du registre est lente (plusieurs secondes par page, bien plus pour une recherche). Au
// premier usage de la bibliothèque, McpHub télécharge le catalogue en arrière-plan, une fois, dans
// ~/.McpHub/registry-cache/catalog.jsonl (une fiche par ligne), puis ne récupère que les fiches
// modifiées (updated_since) au plus une fois par jour. La recherche lit ce fichier à la volée :
// instantanée, hors ligne, et rien ne reste en mémoire entre deux recherches.

const SYNC_EVERY_SECS: u64 = 24 * 3600;
static SYNC_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SYNC_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn catalog_path() -> PathBuf {
    cache_dir().join("catalog.jsonl")
}

fn popular_path() -> PathBuf {
    cache_dir().join("popular.jsonl")
}

fn partial_path() -> PathBuf {
    cache_dir().join("catalog.partial.jsonl")
}

fn state_path() -> PathBuf {
    cache_dir().join("catalog-state.json")
}

fn load_state() -> Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_state(state: &Value) {
    let _ = std::fs::create_dir_all(cache_dir());
    let tmp = state_path().with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, state.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, state_path());
    }
}

/// État de la synchronisation, pour le tableau de bord.
pub fn sync_status() -> Value {
    let st = load_state();
    json!({
        "running": SYNC_RUNNING.load(std::sync::atomic::Ordering::SeqCst),
        "complete": st.get("complete").and_then(|v| v.as_bool()).unwrap_or(false),
        "count": st.get("count").cloned().unwrap_or(json!(0)),
        "downloaded": SYNC_DONE.load(std::sync::atomic::Ordering::SeqCst),
        "popular": st.get("popularCount").cloned().unwrap_or(json!(0)),
        "syncedAt": st.get("syncedAt").cloned().unwrap_or(Value::Null),
        "error": st.get("error").cloned().unwrap_or(Value::Null),
    })
}

/// Lance la synchronisation en arrière-plan si le catalogue manque ou date de plus d'un jour.
pub fn ensure_sync() {
    let st = load_state();
    let complete = st.get("complete").and_then(|v| v.as_bool()).unwrap_or(false);
    let age = now_secs().saturating_sub(st.get("syncedEpoch").and_then(|v| v.as_u64()).unwrap_or(0));
    let popular_age = now_secs().saturating_sub(st.get("popularEpoch").and_then(|v| v.as_u64()).unwrap_or(0));
    if complete && age < SYNC_EVERY_SECS && popular_age < SYNC_EVERY_SECS {
        return;
    }
    if SYNC_RUNNING
        .compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let run = async {
        if let Err(e) = sync_popular().await {
            eprintln!("[McpHub][LIBRARY] Registre MCP de GitHub indisponible : {}", e);
        }
        let res = sync_catalog().await;
        let mut st = load_state();
        match res {
            Ok(n) => {
                st["error"] = Value::Null;
                eprintln!("[McpHub][LIBRARY] Catalogue du registre a jour ({} serveurs)", n);
            }
            Err(e) => {
                st["error"] = json!(e);
                eprintln!("[McpHub][LIBRARY] Synchronisation du catalogue interrompue : {}", e);
            }
        }
        save_state(&st);
        SYNC_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(run);
        }
        Err(_) => SYNC_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst),
    }
}

async fn fetch_page(url: &str) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 0..4u64 {
        let req = http().get(url).header("Accept", "application/json").timeout(Duration::from_secs(90));
        match req.send().await {
            Ok(r) if r.status().is_success() => return r.json::<Value>().await.map_err(|e| e.to_string()),
            Ok(r) => last = format!("HTTP {}", r.status().as_u16()),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(2 + attempt * 3)).await;
    }
    Err(last)
}

fn entry_name(e: &Value) -> &str {
    e.pointer("/server/name").and_then(|v| v.as_str()).unwrap_or("")
}

/// Allège une fiche du Registre MCP de GitHub : sans le README (90 % du poids).
fn slim_github_entry(e: &mut Value) {
    if let Some(g) = e
        .get_mut("server")
        .and_then(|s| s.get_mut("_meta"))
        .and_then(|m| m.get_mut(PUBLISHER))
        .and_then(|p| p.get_mut("github"))
        .and_then(|g| g.as_object_mut())
    {
        let keep = ["stargazerCount", "pushedAt", "license", "ownerAvatarUrl", "nameWithOwner", "topics", "isInOrganization", "displayName"];
        g.retain(|k, _| keep.contains(&k.as_str()));
    }
}

/// Serveurs populaires du Registre MCP de GitHub (quelques centaines, quelques secondes).
async fn sync_popular() -> Result<usize, String> {
    let st = load_state();
    let age = now_secs().saturating_sub(st.get("popularEpoch").and_then(|v| v.as_u64()).unwrap_or(0));
    if age < SYNC_EVERY_SECS && popular_path().exists() {
        return Ok(st.get("popularCount").and_then(|v| v.as_u64()).unwrap_or(0) as usize);
    }
    let mut out = String::new();
    let mut n = 0;
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut url = format!("{}?limit=100", GITHUB_API);
        if let Some(c) = &cursor {
            url.push_str(&format!("&cursor={}", enc(c)));
        }
        let page = fetch_page(&url).await?;
        for e in page.get("servers").and_then(|v| v.as_array()).into_iter().flatten() {
            if status(e) == "deleted" || entry_name(e).is_empty() {
                continue;
            }
            let mut e = e.clone();
            slim_github_entry(&mut e);
            out.push_str(&e.to_string());
            out.push('\n');
            n += 1;
        }
        cursor = page.pointer("/metadata/nextCursor").and_then(|v| v.as_str()).map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    let _ = std::fs::create_dir_all(cache_dir());
    let tmp = popular_path().with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, popular_path()).map_err(|e| e.to_string())?;
    let mut st = load_state();
    st["popularCount"] = json!(n);
    st["popularEpoch"] = json!(now_secs());
    save_state(&st);
    Ok(n)
}

/// Téléchargement complet (reprend où il s'était arrêté) ou mise à jour depuis la dernière fois.
async fn sync_catalog() -> Result<usize, String> {
    use std::io::Write;
    let _ = std::fs::create_dir_all(cache_dir());
    let mut st = load_state();
    let started_iso = crate::audit::iso_utc_now();
    let started = now_secs();
    let complete = st.get("complete").and_then(|v| v.as_bool()).unwrap_or(false) && catalog_path().exists();
    SYNC_DONE.store(0, std::sync::atomic::Ordering::SeqCst);

    if !complete {
        let mut cursor = st.get("cursor").and_then(|v| v.as_str()).map(String::from);
        if cursor.is_none() {
            let _ = std::fs::remove_file(partial_path());
            st["fullStartedAt"] = json!(started_iso);
            st["fullStartedEpoch"] = json!(started);
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(partial_path())
            .map_err(|e| e.to_string())?;
        let mut done = std::fs::read_to_string(partial_path()).map(|c| c.lines().count()).unwrap_or(0);
        SYNC_DONE.store(done, std::sync::atomic::Ordering::SeqCst);
        loop {
            let mut url = format!("{}?version=latest&limit=100", API);
            if let Some(c) = &cursor {
                url.push_str(&format!("&cursor={}", enc(c)));
            }
            let page = fetch_page(&url).await?;
            let mut lines = String::new();
            for e in page.get("servers").and_then(|v| v.as_array()).into_iter().flatten() {
                if status(e) != "deleted" && !entry_name(e).is_empty() {
                    lines.push_str(&e.to_string());
                    lines.push('\n');
                    done += 1;
                }
            }
            out.write_all(lines.as_bytes()).map_err(|e| e.to_string())?;
            SYNC_DONE.store(done, std::sync::atomic::Ordering::SeqCst);
            cursor = page.pointer("/metadata/nextCursor").and_then(|v| v.as_str()).map(String::from);
            st["cursor"] = cursor.clone().map(Value::String).unwrap_or(Value::Null);
            st["count"] = json!(done);
            save_state(&st);
            if cursor.is_none() {
                break;
            }
        }
        drop(out);
        // Une page reprise après une interruption peut figurer deux fois : une fiche par serveur.
        let text = std::fs::read_to_string(partial_path()).map_err(|e| e.to_string())?;
        let mut seen = std::collections::HashSet::new();
        let mut unique: Vec<&str> = Vec::new();
        for line in text.lines().rev() {
            let name = serde_json::from_str::<Slim>(line).map(|x| x.server.name).unwrap_or_default();
            if !name.is_empty() && seen.insert(name) {
                unique.push(line);
            }
        }
        unique.reverse();
        done = unique.len();
        std::fs::write(catalog_path(), unique.join("\n") + "\n").map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(partial_path());
        st["count"] = json!(done);
        // Les fiches modifiées pendant le téléchargement seront reprises à la prochaine mise à jour.
        st["complete"] = json!(true);
        st["syncedAt"] = st.get("fullStartedAt").cloned().unwrap_or(json!(started_iso));
        st["syncedEpoch"] = st.get("fullStartedEpoch").cloned().unwrap_or(json!(started));
        st["cursor"] = Value::Null;
        save_state(&st);
        return Ok(done);
    }

    // Mise à jour : seulement les fiches publiées ou modifiées depuis la dernière synchronisation.
    let since = st.get("syncedAt").and_then(|v| v.as_str()).unwrap_or("1970-01-01T00:00:00Z").to_string();
    let mut changed: HashMap<String, Value> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut url = format!("{}?limit=100&updated_since={}", API, enc(&since));
        if let Some(c) = &cursor {
            url.push_str(&format!("&cursor={}", enc(c)));
        }
        let page = fetch_page(&url).await?;
        for e in page.get("servers").and_then(|v| v.as_array()).into_iter().flatten() {
            let latest = meta(e, "isLatest").and_then(|v| v.as_bool()).unwrap_or(true);
            if latest || status(e) == "deleted" {
                changed.insert(entry_name(e).to_string(), e.clone());
            }
        }
        SYNC_DONE.store(changed.len(), std::sync::atomic::Ordering::SeqCst);
        cursor = page.pointer("/metadata/nextCursor").and_then(|v| v.as_str()).map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    let old = std::fs::read_to_string(catalog_path()).unwrap_or_default();
    let mut out = String::with_capacity(old.len() + changed.len() * 2048);
    let mut count = 0;
    for line in old.lines() {
        let name = serde_json::from_str::<Slim>(line).map(|x| x.server.name).unwrap_or_default();
        if name.is_empty() || changed.contains_key(&name) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        count += 1;
    }
    for e in changed.values() {
        if status(e) != "deleted" {
            out.push_str(&e.to_string());
            out.push('\n');
            count += 1;
        }
    }
    let tmp = catalog_path().with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, catalog_path()).map_err(|e| e.to_string())?;
    st["syncedAt"] = json!(started_iso);
    st["syncedEpoch"] = json!(started);
    st["count"] = json!(count);
    save_state(&st);
    Ok(count)
}

#[derive(serde::Deserialize, Default)]
struct SlimRepo {
    #[serde(default)]
    url: String,
}

#[derive(serde::Deserialize, Default)]
struct SlimGithub {
    #[serde(default, rename = "stargazerCount")]
    stars: u64,
}

#[derive(serde::Deserialize, Default)]
struct SlimPublisher {
    #[serde(default)]
    github: SlimGithub,
}

#[derive(serde::Deserialize, Default)]
struct SlimServerMeta {
    #[serde(default, rename = "io.modelcontextprotocol.registry/publisher-provided")]
    publisher: SlimPublisher,
}

#[derive(serde::Deserialize, Default)]
struct SlimServer {
    #[serde(default)]
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    repository: SlimRepo,
    #[serde(default, rename = "_meta")]
    meta: SlimServerMeta,
}

#[derive(serde::Deserialize, Default)]
struct SlimOfficial {
    #[serde(default, rename = "updatedAt")]
    updated_at: String,
    #[serde(default)]
    status: String,
}

#[derive(serde::Deserialize, Default)]
struct SlimMeta {
    #[serde(default, rename = "io.modelcontextprotocol.registry/official")]
    official: SlimOfficial,
}

#[derive(serde::Deserialize, Default)]
struct Slim {
    #[serde(default)]
    server: SlimServer,
    #[serde(default, rename = "_meta")]
    meta: SlimMeta,
}

/// Lignes du catalogue local (complet, sinon la partie déjà téléchargée).
fn catalog_text() -> Option<String> {
    // Populaires d'abord (fiches les plus riches), puis le catalogue officiel complet ou partiel.
    let mut text = std::fs::read_to_string(popular_path()).unwrap_or_default();
    let full = std::fs::read_to_string(catalog_path())
        .ok()
        .or_else(|| std::fs::read_to_string(partial_path()).ok())
        .unwrap_or_default();
    text.push_str(&full);
    (!text.trim().is_empty()).then_some(text)
}

/// Pertinence d'une fiche pour des mots : tous les mots doivent apparaître quelque part.
fn relevance(x: &SlimServer, words: &[String]) -> Option<u32> {
    // "io.github." est l'espace de noms de tous les éditeurs GitHub : sans valeur pour la recherche.
    let full = x.name.to_lowercase();
    let name = full.strip_prefix("io.github.").unwrap_or(&full).to_string();
    let short = short_name(&name).to_string();
    let title = x.title.to_lowercase();
    let desc = x.description.to_lowercase();
    let mut score = 0;
    for w in words {
        let mut hit = 0;
        if short == *w || title == *w {
            hit += 12;
        }
        if short.contains(w.as_str()) {
            hit += 6;
        } else if name.contains(w.as_str()) {
            hit += 3;
        }
        if title.contains(w.as_str()) {
            hit += 5;
        }
        if desc.contains(w.as_str()) {
            hit += 1;
        }
        if hit == 0 {
            return None;
        }
        score += hit;
    }
    Some(score)
}

fn local_search(text: &str, query: &str, offset: usize, limit: usize) -> (Vec<Value>, usize) {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    let lines: Vec<&str> = text.lines().collect();
    let mut hits: Vec<(u32, String, usize)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, line) in lines.iter().enumerate() {
        let Ok(x) = serde_json::from_str::<Slim>(line) else { continue };
        if x.meta.official.status == "deleted" {
            continue;
        }
        // Même serveur listé par les deux registres : une seule carte (la plus riche, la première).
        let repo_key = format!("{}#{}", x.server.repository.url.to_lowercase().trim_end_matches(".git"), short_name(&x.server.name).to_lowercase());
        if !seen.insert(x.server.name.clone()) || (!x.server.repository.url.is_empty() && !seen.insert(repo_key)) {
            continue;
        }
        let score = if words.is_empty() { Some(0) } else { relevance(&x.server, &words) };
        if let Some(sc) = score {
            // Popularité : 100 étoiles +24, 1000 +36, 100 000 +60 (un serveur reconnu passe devant les homonymes).
            let boost = ((x.server.meta.publisher.github.stars as f64 + 1.0).log10() * 12.0) as u32;
            hits.push((sc * 2 + boost, x.meta.official.updated_at, i));
        }
    }
    // Les plus pertinents, puis les plus récents.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let total = hits.len();
    let page = hits
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|(_, _, i)| serde_json::from_str::<Value>(lines[i]).ok())
        .collect();
    (page, total)
}

fn local_entry(name: &str) -> Option<Value> {
    let text = catalog_text()?;
    let needle = format!("\"name\":{}", serde_json::to_string(name).ok()?);
    text.lines()
        .filter(|l| l.contains(&needle))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| entry_name(e) == name)
}

/// Recherche dans le catalogue (dernière version de chaque serveur). Catalogue local s'il existe,
/// sinon l'API du registre en direct (lente) pendant le premier téléchargement.
pub async fn search(query: &str, cursor: Option<&str>, limit: usize) -> Result<Value, String> {
    let limit = limit.clamp(1, 100);
    ensure_sync();
    let installed = installed_index();
    let complete = load_state().get("complete").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(text) = catalog_text() {
        let offset: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let (page, total) = local_search(&text, query, offset, limit);
        // Catalogue encore partiel et rien trouvé : on demande au registre en direct.
        if page.is_empty() && !complete && !query.trim().is_empty() && offset == 0 {
            if let Ok(live) = live_search(query, limit, &installed).await {
                return Ok(live);
            }
        }
        let next = (offset + page.len() < total).then(|| (offset + page.len()).to_string());
        return Ok(json!({
            "servers": page.iter().map(|e| summary(e, &installed)).collect::<Vec<_>>(),
            "total": total,
            "nextCursor": next,
            "source": "local",
            "sync": sync_status(),
        }));
    }
    live_search_page(query, cursor, limit, &installed).await
}

async fn live_search(query: &str, limit: usize, installed: &HashMap<String, String>) -> Result<Value, String> {
    live_search_page(query, None, limit, installed).await
}

async fn live_search_page(query: &str, cursor: Option<&str>, limit: usize, installed: &HashMap<String, String>) -> Result<Value, String> {
    let mut url = format!("{}?version=latest&limit={}", API, limit);
    if !query.trim().is_empty() {
        url.push_str(&format!("&search={}", enc(query.trim())));
    }
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        url.push_str(&format!("&cursor={}", enc(c)));
    }
    let (data, stale) = cached_json("list", &url, LIST_TTL_SECS, &[]).await?;
    // Chaque fiche reçue sert aussi de cache pour `entry` : ouvrir la fiche ensuite est immédiat.
    for e in data.get("servers").and_then(|s| s.as_array()).into_iter().flatten() {
        if !entry_name(e).is_empty() {
            cache_put(&cache_file("entry", &entry_url(entry_name(e))), e);
        }
    }
    let servers: Vec<Value> = data
        .get("servers")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter(|e| status(e) != "deleted").map(|e| summary(e, installed)).collect())
        .unwrap_or_default();
    Ok(json!({
        "servers": servers,
        "nextCursor": data.pointer("/metadata/nextCursor").cloned().unwrap_or(Value::Null),
        "source": "live",
        "stale": stale,
        "sync": sync_status(),
    }))
}

fn entry_url(name: &str) -> String {
    format!("{}/{}/versions/latest", API, enc(name))
}

/// Fiche complète de la dernière version d'un serveur ({server, _meta}).
pub async fn entry(name: &str) -> Result<Value, String> {
    if let Some(e) = local_entry(name) {
        return Ok(e);
    }
    let url = entry_url(name);
    let (v, _) = cached_json("entry", &url, ENTRY_TTL_SECS, &[]).await?;
    if v.get("server").is_some() {
        Ok(v)
    } else {
        Err("fiche du registre illisible".into())
    }
}

fn short_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Métadonnées GitHub fournies par le Registre MCP de GitHub (étoiles, activité, licence).
fn gh_meta(server: &Value) -> Option<&Value> {
    server.get("_meta").and_then(|m| m.get(PUBLISHER)).and_then(|p| p.get("github"))
}

fn icon(server: &Value) -> Value {
    if let Some(a) = gh_meta(server).and_then(|g| g.get("ownerAvatarUrl")).and_then(|v| v.as_str()) {
        if a.starts_with("https://") && server.get("icons").is_none() {
            return json!(a);
        }
    }
    server
        .get("icons")
        .and_then(|i| i.as_array())
        .and_then(|a| {
            a.iter()
                .filter_map(|i| i.get("src").and_then(|s| s.as_str()))
                .find(|src| src.starts_with("https://"))
        })
        .map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null)
}

fn kinds(server: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if server.get("remotes").and_then(|r| r.as_array()).map_or(false, |a| !a.is_empty()) {
        out.push("remote".into());
    }
    for p in server.get("packages").and_then(|p| p.as_array()).into_iter().flatten() {
        let k = s(p, "registryType").to_string();
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// Résumé d'une fiche pour la liste (score de confiance sans les statistiques du dépôt).
fn summary(entry: &Value, installed: &HashMap<String, String>) -> Value {
    let server = &entry["server"];
    let name = s(server, "name");
    let title = server
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| short_name(name));
    json!({
        "name": name,
        "title": title,
        "description": s(server, "description"),
        "version": s(server, "version"),
        "repository": server.pointer("/repository/url").cloned().unwrap_or(Value::Null),
        "website": server.get("websiteUrl").cloned().unwrap_or(Value::Null),
        "icon": icon(server),
        "updatedAt": meta(entry, "updatedAt").cloned().unwrap_or(Value::Null),
        "status": status(entry),
        "kinds": kinds(server),
        "stars": gh_meta(server).and_then(|g| g.get("stargazerCount")).cloned().unwrap_or(Value::Null),
        "installed": installed.get(name),
        "trust": trust(entry, None),
    })
}

/// Serveurs déjà installés depuis le registre : nom du registre -> nom local.
fn installed_index() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let config = read_hub_config().unwrap_or_else(|_| json!({}));
    if let Some(servers) = servers_obj(&config) {
        for (local, srv) in servers {
            if let Some(n) = srv.pointer("/registry/name").and_then(|v| v.as_str()) {
                out.insert(n.to_string(), local.clone());
            }
        }
    }
    out
}

// ─── Dépôt GitHub (popularité, activité) ─────────────────────

fn github_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    Some((owner, repo))
}

/// Étoiles, dernier push, archivage du dépôt GitHub (cache 24 h). Jeton GITHUB_TOKEN utilisé
/// s'il est défini (5000 requêtes/h au lieu de 60), envoyé uniquement à api.github.com.
pub async fn repo_stats(repo_url: &str) -> Option<Value> {
    let (owner, repo) = github_repo(repo_url)?;
    let api = format!("https://api.github.com/repos/{}/{}", owner, repo);
    let mut headers = vec![("Accept", "application/vnd.github+json".to_string())];
    if let Some(t) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .filter(|t| !t.is_empty() && !t.contains("${"))
    {
        headers.push(("Authorization", format!("Bearer {}", t)));
    }
    let (v, _) = cached_json("repo", &api, REPO_TTL_SECS, &headers).await.ok()?;
    Some(json!({
        "stars": v.get("stargazers_count").cloned().unwrap_or(Value::Null),
        "forks": v.get("forks_count").cloned().unwrap_or(Value::Null),
        "pushedAt": v.get("pushed_at").cloned().unwrap_or(Value::Null),
        "archived": v.get("archived").and_then(|a| a.as_bool()).unwrap_or(false),
        "license": v.pointer("/license/spdx_id").cloned().unwrap_or(Value::Null),
        "owner": v.pointer("/owner/login").cloned().unwrap_or(Value::Null),
    }))
}

// ─── Score de confiance ──────────────────────────────────────

/// Jours écoulés depuis une date ISO 8601 (seule la date compte).
fn days_since(iso: &str) -> Option<u64> {
    let date = iso.get(..10)?;
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    // Jours depuis 1970-01-01 (algorithme de Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let today = (now_secs() / 86400) as i64;
    Some((today - days).max(0) as u64)
}

/// Domaine d'un espace de noms inversé : "com.stripe" -> "stripe.com".
fn namespace_domain(ns: &str) -> String {
    let mut labels: Vec<&str> = ns.split('.').collect();
    labels.reverse();
    labels.join(".")
}

const DANGEROUS_DOCKER: &[(&str, &str)] = &[
    ("--privileged", "privileged container"),
    ("--network=host", "host network"),
    ("--net=host", "host network"),
    ("--pid=host", "host processes"),
    ("--cap-add", "extra kernel capabilities"),
    ("docker.sock", "control of the Docker daemon"),
    ("-v /:", "the whole disk mounted"),
    ("--volume=/:", "the whole disk mounted"),
];

const CAPABILITIES: &[(&[&str], &str)] = &[
    (&["shell", "terminal", "command", "exec", "bash", "ssh"], "Can run commands"),
    (&["filesystem", "file system", "files", "directory", "folder"], "Reads or writes files"),
    (&["browser", "chrome", "playwright", "puppeteer", "selenium"], "Controls a browser"),
    (&["email", "gmail", "slack", "discord", "telegram", "sms", "whatsapp"], "Can send messages"),
    (&["payment", "stripe", "bank", "wallet", "invoice", "crypto", "trading"], "Touches money or payments"),
    (&["database", "sql", "postgres", "mysql", "mongodb", "supabase", "redis"], "Database access"),
    (&["deploy", "kubernetes", "aws", "cloud", "terraform", "docker"], "Manages infrastructure"),
];

/// Score de confiance (0-100) avec le détail de chaque signal. `repo` = statistiques GitHub
/// (None : popularité et activité du dépôt pas encore vérifiées, score partiel).
pub fn trust(entry: &Value, repo: Option<&Value>) -> Value {
    let server = &entry["server"];
    let name = s(server, "name");
    let mut score: i64 = 0;
    let mut signals: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut point = |label: String, pts: i64, signals: &mut Vec<Value>| {
        score += pts;
        signals.push(json!({"label": label, "points": pts}));
    };

    match status(entry) {
        "active" => point("Active in the official MCP Registry".into(), 10, &mut signals),
        "deprecated" => {
            point("Deprecated by its publisher".into(), -30, &mut signals);
            warnings.push("Deprecated: the publisher recommends another server".into());
        }
        other => warnings.push(format!("Registry status: {}", other)),
    }

    // Éditeur : l'espace de noms est vérifié par le registre (compte GitHub ou domaine DNS).
    let repo_url = server.pointer("/repository/url").and_then(|v| v.as_str()).unwrap_or("");
    let repo_owner = github_repo(repo_url).map(|(o, _)| o);
    let ns = name.split('/').next().unwrap_or("");
    if let Some(gh) = ns.strip_prefix("io.github.") {
        match &repo_owner {
            Some(o) if o.eq_ignore_ascii_case(gh) => point(
                format!("Verified publisher: GitHub account {} publishes its own code", gh),
                30,
                &mut signals,
            ),
            Some(o) => point(
                format!("Republished by {}: the code belongs to {}", gh, o),
                10,
                &mut signals,
            ),
            None => point(format!("Verified GitHub account {}", gh), 15, &mut signals),
        }
    } else if !ns.is_empty() {
        // Domaine vérifié par DNS ou HTTP au moment de la publication.
        let domain = namespace_domain(ns);
        let brand = ns.split('.').nth(1).unwrap_or("").to_lowercase();
        // Un domaine vérifié ne dit rien de la qualité : il compte surtout quand le code public
        // appartient au même éditeur.
        match &repo_owner {
            Some(o) if !brand.is_empty() && o.to_lowercase().contains(&brand) => point(
                format!("Verified domain {}, publishing its own code", domain),
                25,
                &mut signals,
            ),
            Some(o) => point(format!("Verified domain {} (code hosted by {})", domain, o), 15, &mut signals),
            None => point(format!("Verified domain {}", domain), 12, &mut signals),
        }
    }
    if repo_url.is_empty() {
        point("No source repository".into(), -10, &mut signals);
        warnings.push("No source repository: the code cannot be inspected".into());
    } else {
        point("Open source repository".into(), 5, &mut signals);
    }

    // Popularité et activité du dépôt.
    let mut latest_days = meta(entry, "updatedAt").and_then(|v| v.as_str()).and_then(days_since);
    // Sans appel à l'API GitHub : les étoiles et l'activité fournies par le Registre MCP de GitHub.
    let from_gh = gh_meta(server).map(|g| {
        json!({
            "stars": g.get("stargazerCount").cloned().unwrap_or(json!(0)),
            "pushedAt": g.get("pushedAt").cloned().unwrap_or(Value::Null),
            "archived": false,
        })
    });
    let repo = repo.or(from_gh.as_ref());
    let complete = repo.is_some() || github_repo(repo_url).is_none();
    if let Some(r) = repo {
        let stars = r.get("stars").and_then(|v| v.as_u64()).unwrap_or(0);
        let pts = match stars {
            1000.. => 20,
            200..=999 => 15,
            50..=199 => 10,
            10..=49 => 5,
            _ => 0,
        };
        point(format!("{} GitHub stars", stars), pts, &mut signals);
        if r.get("archived").and_then(|v| v.as_bool()).unwrap_or(false) {
            point("Repository archived".into(), -25, &mut signals);
            warnings.push("The repository is archived: no more fixes".into());
        }
        if let Some(d) = r.get("pushedAt").and_then(|v| v.as_str()).and_then(days_since) {
            latest_days = Some(latest_days.map_or(d, |l| l.min(d)));
        }
    }
    match latest_days {
        Some(d) if d <= 90 => point(format!("Updated {} days ago", d), 15, &mut signals),
        Some(d) if d <= 365 => point(format!("Updated {} days ago", d), 8, &mut signals),
        Some(d) => {
            point(format!("No update for {} days", d), 0, &mut signals);
            warnings.push("No update for over a year".into());
        }
        None => {}
    }

    // Analyse statique de ce que le serveur demande.
    let remotes: Vec<&Value> = server.get("remotes").and_then(|r| r.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    let packages: Vec<&Value> = server.get("packages").and_then(|p| p.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    if !remotes.is_empty() {
        if remotes.iter().all(|r| s(r, "url").starts_with("https://")) {
            point("Runs remotely over HTTPS: no code on this machine".into(), 10, &mut signals);
        } else {
            point("Unencrypted HTTP endpoint".into(), -15, &mut signals);
            warnings.push("A remote endpoint is plain HTTP".into());
        }
    } else if let Some(p) = packages.first() {
        match s(p, "registryType") {
            "mcpb" if !s(p, "fileSha256").is_empty() => point("Bundle with an integrity hash (SHA-256)".into(), 10, &mut signals),
            "mcpb" => point("Bundle without an integrity hash".into(), -10, &mut signals),
            _ if !s(p, "version").is_empty() => point("Pinned package version".into(), 5, &mut signals),
            _ => point("Unpinned package version".into(), -5, &mut signals),
        }
    }
    for p in &packages {
        let rt: Vec<String> = arg_strings(p.get("runtimeArguments"));
        let joined = rt.join(" ");
        for (flag, what) in DANGEROUS_DOCKER {
            if joined.contains(flag) {
                point(format!("Docker option {}", flag), -25, &mut signals);
                warnings.push(format!("The container asks for {}", what));
            }
        }
    }

    // Capacités annoncées (information, sans points).
    let mut capabilities: Vec<String> = Vec::new();
    let secrets = secret_names(server);
    if !secrets.is_empty() {
        capabilities.push(format!("Asks for {} secret{}: {}", secrets.len(), if secrets.len() > 1 { "s" } else { "" }, secrets.join(", ")));
    }
    let text = format!("{} {} {}", name, s(server, "title"), s(server, "description")).to_lowercase();
    for (words, label) in CAPABILITIES {
        if words.iter().any(|w| text.contains(w)) {
            capabilities.push(label.to_string());
        }
    }

    let score = score.clamp(0, 100);
    let level = if score >= 75 { "high" } else if score >= 50 { "medium" } else { "low" };
    json!({
        "score": score,
        "level": level,
        "complete": complete,
        "signals": signals,
        "warnings": warnings,
        "capabilities": capabilities,
    })
}

fn arg_strings(v: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    for a in v.and_then(|x| x.as_array()).into_iter().flatten() {
        for k in ["name", "value", "default"] {
            if let Some(x) = a.get(k).and_then(|x| x.as_str()) {
                out.push(x.to_string());
            }
        }
    }
    out
}

/// Noms des secrets demandés (variables, en-têtes, arguments), toutes options confondues.
fn secret_names(server: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: &str| {
        if !n.is_empty() && !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    };
    for p in server.get("packages").and_then(|p| p.as_array()).into_iter().flatten() {
        for e in p.get("environmentVariables").and_then(|e| e.as_array()).into_iter().flatten() {
            if e.get("isSecret").and_then(|v| v.as_bool()).unwrap_or(false) {
                push(s(e, "name"));
            }
            for (k, v) in e.get("variables").and_then(|v| v.as_object()).into_iter().flatten() {
                if v.get("isSecret").and_then(|x| x.as_bool()).unwrap_or(false) {
                    push(k);
                }
            }
        }
    }
    for r in server.get("remotes").and_then(|r| r.as_array()).into_iter().flatten() {
        for h in r.get("headers").and_then(|h| h.as_array()).into_iter().flatten() {
            if h.get("isSecret").and_then(|v| v.as_bool()).unwrap_or(false) {
                for v in placeholders(s(h, "value")) {
                    push(&v);
                }
                if placeholders(s(h, "value")).is_empty() {
                    push(s(h, "name"));
                }
            }
        }
    }
    out
}

/// Noms entre accolades d'un modèle : "Bearer {api_key}" -> ["api_key"].
fn placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
                    out.push(name.to_string());
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

/// Fiche + statistiques du dépôt + score complet + options d'installation, pour le tableau de bord.
pub async fn details(name: &str) -> Result<Value, String> {
    let e = entry(name).await?;
    let repo_url = e.pointer("/server/repository/url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stats = if repo_url.is_empty() || gh_meta(&e["server"]).is_some() {
        gh_meta(&e["server"]).map(|g| json!({
            "stars": g.get("stargazerCount").cloned().unwrap_or(Value::Null),
            "pushedAt": g.get("pushedAt").cloned().unwrap_or(Value::Null),
            "archived": false,
            "license": g.get("license").cloned().unwrap_or(Value::Null),
        }))
    } else {
        repo_stats(&repo_url).await
    };
    let installed = installed_index();
    let mut out = summary(&e, &installed);
    out["trust"] = trust(&e, stats.as_ref());
    out["repo"] = stats.unwrap_or(Value::Null);
    out["options"] = Value::Array(options(&e["server"]));
    Ok(out)
}

// ─── Commandes locales ───────────────────────────────────────

fn exe_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{}.exe", name), format!("{}.cmd", name), name.to_string()]
    } else {
        vec![name.to_string()]
    }
}

/// Programme disponible sur cette machine. Trouvé dans le PATH : son nom (config portable d'une
/// machine à l'autre) ; trouvé ailleurs (Homebrew, nvm, ~/.local/bin...) : son chemin complet.
pub fn find_cmd(name: &str) -> Option<String> {
    if name.contains('/') || name.contains('\\') {
        return Path::new(name).exists().then(|| name.to_string());
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for n in exe_names(name) {
            if dir.join(&n).is_file() {
                return Some(name.to_string());
            }
        }
    }
    let home = dirs::home_dir().unwrap_or_default();
    let mut extra: Vec<PathBuf> = vec![
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".dotnet/tools"),
    ];
    // nvm : la version la plus récente.
    if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        versions.sort();
        if let Some(v) = versions.last() {
            extra.push(v.join("bin"));
        }
    }
    for dir in extra {
        for n in exe_names(name) {
            let p = dir.join(&n);
            if p.is_file() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn os_key() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

// ─── Construction de la configuration ────────────────────────

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Nom de variable d'environnement : SLUG_LABEL en majuscules.
fn env_var_name(slug: &str, label: &str) -> String {
    let raw = format!("{}_{}", slug, label.trim_start_matches('-'));
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.chars().next().map_or(true, |c| c.is_ascii_digit()) {
        format!("MCP_{}", out)
    } else {
        out
    }
}

struct Builder<'a> {
    slug: String,
    values: &'a Map<String, Value>,
    /// Secrets à enregistrer au Trousseau : (nom, valeur).
    secrets: Vec<(String, String)>,
    /// Tout ce qui peut être demandé à l'utilisateur.
    inputs: Vec<Value>,
    /// Clés obligatoires sans valeur.
    needs: Vec<String>,
    known: Vec<String>,
}

impl<'a> Builder<'a> {
    fn new(slug: &str, values: &'a Map<String, Value>) -> Self {
        Self {
            slug: slug.to_string(),
            values,
            secrets: Vec::new(),
            inputs: Vec::new(),
            needs: Vec::new(),
            known: crate::vault::list_names(),
        }
    }

    fn known_secret(&self, var: &str) -> bool {
        std::env::var(var).map_or(false, |v| !v.is_empty()) || self.known.iter().any(|k| k == var)
    }

    /// Valeur d'une entrée : littérale, ou `${VAR}` pour un secret (rangé au Trousseau).
    fn input(&mut self, key: &str, label: &str, spec: &Value, env_name: Option<&str>) -> Option<String> {
        let secret = flag(spec, "isSecret");
        let required = flag(spec, "isRequired");
        let default = spec.get("default").and_then(|v| v.as_str()).filter(|v| !v.is_empty());
        let var = env_name.map(String::from).unwrap_or_else(|| env_var_name(&self.slug, label));
        let provided = self
            .values
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(String::from);
        let existing = secret && self.known_secret(&var);
        if !self.inputs.iter().any(|i| i["key"] == key) {
            self.inputs.push(json!({
                "key": key,
                "label": label,
                "description": spec.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "secret": secret,
                "required": required && !existing,
                "default": if secret { Value::Null } else { default.map(|d| json!(d)).unwrap_or(Value::Null) },
                "choices": spec.get("choices").cloned().unwrap_or(Value::Null),
                "existing": existing,
                "variable": if secret { json!(var) } else { Value::Null },
            }));
        }
        if secret {
            if let Some(v) = provided {
                // Une valeur différente d'un secret déjà connu ne l'écrase jamais : elle prend
                // un nom propre à ce serveur.
                let target = if self.known_secret(&var) && std::env::var(&var).ok().as_deref() != Some(v.as_str()) {
                    env_var_name(&self.slug, label)
                } else {
                    var.clone()
                };
                self.secrets.push((target.clone(), v));
                return Some(format!("${{{}}}", target));
            }
            if existing {
                return Some(format!("${{{}}}", var));
            }
        } else if let Some(v) = provided.or(default.map(String::from)) {
            return Some(v);
        }
        if required && !self.needs.iter().any(|n| n == key) {
            self.needs.push(key.to_string());
        }
        None
    }

    /// Modèle "Bearer {api_key}" : chaque {nom} devient une entrée (secrète si le parent l'est).
    fn template(&mut self, tpl: &str, vars: Option<&Value>, parent: &Value) -> Option<String> {
        let mut out = tpl.to_string();
        let mut complete = true;
        for p in placeholders(tpl) {
            let mut spec = vars
                .and_then(|v| v.get(&p))
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            // Une variable d'un modèle est indispensable ; elle hérite du caractère secret du parent.
            if spec.get("isSecret").is_none() {
                spec["isSecret"] = json!(flag(parent, "isSecret"));
            }
            if spec.get("isRequired").is_none() {
                spec["isRequired"] = json!(true);
            }
            if spec.get("description").is_none() {
                spec["description"] = parent.get("description").cloned().unwrap_or(Value::Null);
            }
            match self.input(&format!("var:{}", p), &p, &spec, None) {
                Some(v) => out = out.replace(&format!("{{{}}}", p), &v),
                None => complete = false,
            }
        }
        complete.then_some(out)
    }

    fn value_of(&mut self, key: &str, label: &str, spec: &Value, env_name: Option<&str>) -> Option<String> {
        match spec.get("value").and_then(|v| v.as_str()) {
            Some(tpl) if !placeholders(tpl).is_empty() => self.template(tpl, spec.get("variables"), spec),
            Some(fixed) => Some(fixed.to_string()),
            None => self.input(key, label, spec, env_name),
        }
    }

    fn args(&mut self, list: Option<&Value>, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, a) in list.and_then(|l| l.as_array()).into_iter().flatten().enumerate() {
            let name = s(a, "name");
            let label = if !name.is_empty() {
                name.to_string()
            } else if !s(a, "valueHint").is_empty() {
                s(a, "valueHint").to_string()
            } else {
                format!("argument {}", i + 1)
            };
            let value = self.value_of(&format!("{}:{}", prefix, label), &label, a, None);
            if s(a, "type") == "named" {
                if let Some(v) = value {
                    out.push(name.to_string());
                    if !v.is_empty() {
                        out.push(v);
                    }
                }
            } else if let Some(v) = value {
                out.push(v);
            }
        }
        out
    }

    fn env(&mut self, list: Option<&Value>) -> Map<String, Value> {
        let mut env = Map::new();
        for e in list.and_then(|l| l.as_array()).into_iter().flatten() {
            let name = s(e, "name");
            if name.is_empty() {
                continue;
            }
            if let Some(v) = self.value_of(&format!("env:{}", name), name, e, Some(name)) {
                env.insert(name.to_string(), json!(v));
            }
        }
        env
    }

    fn package(&mut self, p: &Value) -> Result<Value, String> {
        let transport = p.pointer("/transport/type").and_then(|v| v.as_str()).unwrap_or("stdio");
        if transport != "stdio" {
            return Err(format!("local {} transport is not supported, only stdio", transport));
        }
        let id = s(p, "identifier");
        let ver = s(p, "version");
        let env = self.env(p.get("environmentVariables"));
        let runtime = self.args(p.get("runtimeArguments"), "runtime");
        let pkg = self.args(p.get("packageArguments"), "arg");
        let hint = s(p, "runtimeHint");
        let pick = |default: &str| if hint.is_empty() { default.to_string() } else { hint.to_string() };
        let (cmd, mut args) = match s(p, "registryType") {
            "npm" => {
                let mut a = if runtime.is_empty() { vec!["-y".to_string()] } else { runtime };
                a.push(if ver.is_empty() { id.to_string() } else { format!("{}@{}", id, ver) });
                (pick("npx"), a)
            }
            "pypi" => {
                let mut a = runtime;
                a.push(if ver.is_empty() { id.to_string() } else { format!("{}=={}", id, ver) });
                (pick("uvx"), a)
            }
            "nuget" => {
                let mut a = vec![if ver.is_empty() { id.to_string() } else { format!("{}@{}", id, ver) }, "--yes".to_string()];
                a.extend(runtime);
                if !pkg.is_empty() {
                    a.push("--".into());
                }
                (pick("dnx"), a)
            }
            "oci" => {
                let mut a: Vec<String> = Vec::new();
                if runtime.first().map(|r| r.as_str()) != Some("run") {
                    a.extend(["run", "-i", "--rm"].map(String::from));
                }
                a.extend(runtime);
                for k in env.keys() {
                    a.push("-e".into());
                    a.push(k.clone());
                }
                a.push(id.to_string());
                (pick("docker"), a)
            }
            other => return Err(format!("{} packages are not supported", other)),
        };
        args.extend(pkg);
        let cmd = find_cmd(&cmd).unwrap_or(cmd);
        Ok(json!({"command": cmd, "args": args, "env": env}))
    }

    fn remote(&mut self, r: &Value) -> Result<Value, String> {
        if s(r, "type") != "streamable-http" {
            return Err(format!("{} remotes are not supported, only streamable-http", s(r, "type")));
        }
        let mut url = s(r, "url").to_string();
        if !placeholders(&url).is_empty() {
            url = self.template(&url, r.get("variables"), &json!({"isRequired": true})).unwrap_or(url);
        }
        let mut headers = Map::new();
        for h in r.get("headers").and_then(|h| h.as_array()).into_iter().flatten() {
            let name = s(h, "name");
            if let Some(v) = self.value_of(&format!("header:{}", name), name, h, None) {
                headers.insert(name.to_string(), json!(v));
            }
        }
        let has_auth = headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"));
        let mut cfg = json!({"url": url});
        if !headers.is_empty() {
            cfg["headers"] = Value::Object(headers);
        }
        cfg["auth"] = json!(if has_auth { "none" } else { "oauth" });
        Ok(cfg)
    }

    /// Bundle .mcpb déjà extrait : commande du manifeste pour ce système, ${user_config.X} résolus.
    fn bundle(&mut self, dir: &Path, manifest: &Value) -> Result<Value, String> {
        let server = manifest.get("server").ok_or("manifest.json sans section server")?;
        let mcp = server.get("mcp_config").cloned().unwrap_or_else(|| json!({}));
        let over = mcp.pointer(&format!("/platform_overrides/{}", os_key())).cloned().unwrap_or_else(|| json!({}));
        let entry_point = s(server, "entry_point");
        let mut command = over
            .get("command")
            .or_else(|| mcp.get("command"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| match s(server, "type") {
                "node" => "node".into(),
                "python" => "python3".into(),
                _ => format!("${{__dirname}}/{}", entry_point),
            });
        let mut args: Vec<String> = over
            .get("args")
            .or_else(|| mcp.get("args"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_else(|| match s(server, "type") {
                "node" | "python" => vec![format!("${{__dirname}}/{}", entry_point)],
                _ => Vec::new(),
            });
        let mut env: Map<String, Value> = mcp.get("env").and_then(|e| e.as_object()).cloned().unwrap_or_default();
        if let Some(o) = over.get("env").and_then(|e| e.as_object()) {
            env.extend(o.clone());
        }
        let user_config = manifest.get("user_config").cloned().unwrap_or_else(|| json!({}));
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
        let sep = if cfg!(windows) { "\\" } else { "/" };
        let dirname = dir.to_string_lossy().to_string();
        let subst = |b: &mut Self, raw: &str| -> String {
            let mut out = raw
                .replace("${__dirname}", &dirname)
                .replace("${HOME}", &home)
                .replace("${pathSeparator}", sep)
                .replace("${/}", sep);
            while let Some(start) = out.find("${user_config.") {
                let Some(end_rel) = out[start..].find('}') else { break };
                let key = out[start + 14..start + end_rel].to_string();
                let uc = user_config.get(&key).cloned().unwrap_or_else(|| json!({}));
                let spec = json!({
                    "isSecret": flag(&uc, "sensitive"),
                    "isRequired": flag(&uc, "required"),
                    "default": uc.get("default").map(|d| match d { Value::String(s) => json!(s), other => json!(other.to_string()) }).unwrap_or(Value::Null),
                    "description": uc.get("description").or_else(|| uc.get("title")).cloned().unwrap_or(Value::Null),
                });
                let label = uc.get("title").and_then(|t| t.as_str()).unwrap_or(&key).to_string();
                let v = b.input(&format!("user:{}", key), &label, &spec, None).unwrap_or_default();
                out.replace_range(start..start + end_rel + 1, &v);
            }
            out
        };
        command = subst(self, &command);
        args = args.iter().map(|a| subst(self, a)).collect();
        let env: Map<String, Value> = env
            .into_iter()
            .map(|(k, v)| (k, json!(subst(self, v.as_str().unwrap_or("")))))
            .collect();
        if command == "node" || command == "python3" || command == "python" || command == "uv" {
            command = find_cmd(&command).unwrap_or(command);
        } else if Path::new(&command).is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755));
            }
        }
        Ok(json!({"command": command, "args": args, "env": env}))
    }
}

// ─── Options d'installation ──────────────────────────────────

fn kind_rank(kind: &str) -> usize {
    ["remote", "mcpb", "npm", "pypi", "oci", "nuget"].iter().position(|k| *k == kind).unwrap_or(9)
}

fn runtime_for(kind: &str, hint: &str) -> Option<&'static str> {
    if !hint.is_empty() {
        return match hint {
            "npx" => Some("npx"),
            "uvx" => Some("uvx"),
            "docker" => Some("docker"),
            "dnx" => Some("dnx"),
            _ => None,
        };
    }
    match kind {
        "npm" => Some("npx"),
        "pypi" => Some("uvx"),
        "oci" => Some("docker"),
        "nuget" => Some("dnx"),
        _ => None,
    }
}

/// Façons d'installer ce serveur ici, la recommandée en premier, avec ce qu'il faudra saisir.
pub fn options(server: &Value) -> Vec<Value> {
    let empty = Map::new();
    let mut out: Vec<Value> = Vec::new();
    for (i, r) in server.get("remotes").and_then(|r| r.as_array()).into_iter().flatten().enumerate() {
        let mut b = Builder::new("x", &empty);
        let res = b.remote(r);
        let host = s(r, "url").split('/').nth(2).unwrap_or("").to_string();
        out.push(json!({
            "id": format!("remote:{}", i),
            "kind": "remote",
            "label": format!("Remote server ({})", host),
            "available": res.is_ok(),
            "reason": res.err(),
            "inputs": b.inputs,
        }));
    }
    for (i, p) in server.get("packages").and_then(|p| p.as_array()).into_iter().flatten().enumerate() {
        let kind = s(p, "registryType");
        let mut reason: Option<String> = None;
        let mut inputs: Vec<Value> = Vec::new();
        if kind == "mcpb" {
            if !s(p, "identifier").starts_with("https://") {
                reason = Some("bundle URL is not HTTPS".into());
            }
        } else if let Some(rt) = runtime_for(kind, s(p, "runtimeHint")) {
            if find_cmd(rt).is_none() {
                reason = Some(match rt {
                    "npx" => "npx not found: install Node.js".to_string(),
                    "uvx" => "uvx not found: install uv (https://docs.astral.sh/uv/)".to_string(),
                    "docker" => "docker not found: install Docker".to_string(),
                    other => format!("{} not found", other),
                });
            }
        } else {
            reason = Some(format!("{} packages are not supported", kind));
        }
        if reason.is_none() && kind != "mcpb" {
            let mut b = Builder::new("x", &empty);
            if let Err(e) = b.package(p) {
                reason = Some(e);
            }
            inputs = b.inputs;
        }
        let label = match kind {
            "npm" => format!("npm package {}", s(p, "identifier")),
            "pypi" => format!("Python package {}", s(p, "identifier")),
            "oci" => format!("Docker image {}", s(p, "identifier")),
            "nuget" => format!(".NET package {}", s(p, "identifier")),
            "mcpb" => "Desktop bundle (.mcpb)".to_string(),
            other => format!("{} package", other),
        };
        out.push(json!({
            "id": format!("package:{}", i),
            "kind": kind,
            "label": label,
            "available": reason.is_none(),
            "reason": reason,
            "inputs": inputs,
        }));
    }
    out.sort_by_key(|o| (!o["available"].as_bool().unwrap_or(false), kind_rank(o["kind"].as_str().unwrap_or(""))));
    out
}

// ─── Bundles .mcpb ───────────────────────────────────────────

/// Bundles installés : hors de ~/.McpHub, que le bac à sable cache aux serveurs (un serveur isolé
/// doit pouvoir lire son propre code). ~/Library/Application Support/McpHub/servers sur macOS,
/// ~/.local/share/McpHub/servers sous Linux ; un dossier par instance nommée.
pub fn servers_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("McpHub");
    match &crate::instance::current().name {
        Some(n) => base.join(format!("servers-{}", n)),
        None => base.join("servers"),
    }
}

/// Télécharge (si besoin), vérifie l'empreinte SHA-256 et extrait le bundle dans
/// ~/.McpHub/servers/<nom>/current. Renvoie le dossier et le manifeste.
async fn prepare_bundle(p: &Value, slug: &str) -> Result<(PathBuf, Value), String> {
    let url = s(p, "identifier");
    if !url.starts_with("https://") {
        return Err("bundle URL must be HTTPS".into());
    }
    let expected = s(p, "fileSha256").to_ascii_lowercase();
    let base = servers_dir().join(slug);
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let file = base.join("bundle.mcpb");
    let current = base.join("current");
    let have = std::fs::read(&file).ok().map(|b| format!("{:x}", Sha256::digest(&b)));
    let reuse = !expected.is_empty() && have.as_deref() == Some(expected.as_str()) && current.join("manifest.json").exists();
    if !reuse {
        let resp = http()
            .get(url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|e| format!("download failed: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("download failed: HTTP {}", resp.status().as_u16()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("download failed: {}", e))?;
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err("bundle larger than 300 MB".into());
        }
        let got = format!("{:x}", Sha256::digest(&bytes));
        if !expected.is_empty() && got != expected {
            return Err(format!("integrity check failed: SHA-256 {} instead of {}", got, expected));
        }
        let tmp = base.join("bundle.mcpb.part");
        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
        let staging = base.join("staging");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let status = if cfg!(windows) {
            std::process::Command::new("tar").arg("-xf").arg(&file).arg("-C").arg(&staging).status()
        } else {
            std::process::Command::new("unzip").arg("-q").arg("-o").arg(&file).arg("-d").arg(&staging).status()
        }
        .map_err(|e| format!("cannot extract the bundle (unzip missing?): {}", e))?;
        if !status.success() {
            return Err("cannot extract the bundle".into());
        }
        // Rien ne doit sortir du dossier du bundle (archive piégée avec des chemins "../").
        let canon = staging.canonicalize().map_err(|e| e.to_string())?;
        if !all_inside(&canon, &canon) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err("bundle contains links outside its folder".into());
        }
        let _ = std::fs::remove_dir_all(&current);
        std::fs::rename(&staging, &current).map_err(|e| e.to_string())?;
    }
    let raw = std::fs::read_to_string(current.join("manifest.json")).map_err(|_| "bundle without manifest.json".to_string())?;
    let manifest: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid manifest.json: {}", e))?;
    Ok((current, manifest))
}

fn all_inside(dir: &Path, root: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return true };
    for e in rd.flatten() {
        let p = e.path();
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.file_type().is_symlink() {
                match p.canonicalize() {
                    Ok(target) if target.starts_with(root) => {}
                    _ => return false,
                }
            } else if meta.is_dir() && !all_inside(&p, root) {
                return false;
            }
        }
    }
    true
}

// ─── Configuration McpHub ────────────────────────────────────

fn hub_config_path() -> PathBuf {
    crate::instance::hub_dir().join("config.json")
}

fn read_hub_config() -> Result<Value, String> {
    let path = hub_config_path();
    if !path.exists() {
        return Ok(json!({"mcpServers": {}}));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&raw).map_err(|e| format!("{} is not valid JSON: {}", path.display(), e))
}

fn servers_key(config: &Value) -> &'static str {
    if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    }
}

fn servers_obj(config: &Value) -> Option<&Map<String, Value>> {
    config.get(servers_key(config)).and_then(|v| v.as_object())
}

fn write_hub_config(config: &Value) -> Result<(), String> {
    let path = hub_config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(config).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    crate::vault::restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Nom local libre dérivé du nom du registre ("io.github.acme/weather-mcp" -> "weather-mcp").
fn local_slug(name: &str, taken: &Map<String, Value>) -> String {
    let base: String = short_name(name)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() || base == "mcp" || base == "server" {
        name.split('/').next().unwrap_or("server").replace('.', "-")
    } else {
        base
    };
    if !taken.contains_key(&base) {
        return base;
    }
    (2..).map(|i| format!("{}-{}", base, i)).find(|n| !taken.contains_key(n)).unwrap()
}

fn store_secret(name: &str, value: &str) -> Result<(), String> {
    crate::vault::set(name, value)?;
    // Le démon résout ${NOM} depuis son environnement : la valeur sert dès maintenant.
    std::env::set_var(name, value);
    Ok(())
}

// ─── Installation ────────────────────────────────────────────

pub struct InstallRequest {
    /// Nom du serveur dans le registre.
    pub name: String,
    /// Option choisie ("remote:0", "package:1"...), sinon la recommandée.
    pub option: Option<String>,
    /// Valeurs saisies, par clé d'entrée ("env:API_KEY", "var:token"...).
    pub values: Map<String, Value>,
    /// Nom local voulu (sinon dérivé du nom du registre).
    pub local_name: Option<String>,
    /// "dashboard", "cli" ou "agent".
    pub by: String,
}

/// Installe un serveur du registre dans ~/.McpHub/config.json. S'il manque des valeurs
/// obligatoires, rien n'est écrit et la réponse liste ce qu'il faut saisir (`needs`).
pub async fn install(req: InstallRequest) -> Result<Value, String> {
    let e = entry(&req.name).await?;
    if status(&e) == "deleted" {
        return Err("this server was removed from the registry".into());
    }
    let server = e["server"].clone();
    let opts = options(&server);
    let chosen = match &req.option {
        Some(id) => opts.iter().find(|o| o["id"] == id.as_str()).cloned().ok_or_else(|| format!("unknown option {}", id))?,
        None => opts.first().cloned().ok_or("nothing installable for this server")?,
    };
    if !chosen["available"].as_bool().unwrap_or(false) {
        return Err(chosen["reason"].as_str().unwrap_or("option not available on this machine").to_string());
    }
    let option_id = chosen["id"].as_str().unwrap_or("").to_string();
    let kind = chosen["kind"].as_str().unwrap_or("").to_string();

    let mut config = read_hub_config()?;
    let key = servers_key(&config);
    if config.get(key).and_then(|v| v.as_object()).is_none() {
        config[key] = json!({});
    }
    let taken = config[key].as_object().cloned().unwrap_or_default();
    let previous = taken
        .iter()
        .find(|(_, v)| v.pointer("/registry/name").and_then(|n| n.as_str()) == Some(req.name.as_str()))
        .map(|(k, _)| k.clone());
    let local = match (&req.local_name, &previous) {
        (Some(n), _) if !n.trim().is_empty() => n.trim().to_string(),
        (_, Some(p)) => p.clone(),
        _ => local_slug(&req.name, &taken),
    };
    if !local.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("server name may only contain letters, digits, - and _".into());
    }
    if previous.as_deref() != Some(local.as_str()) && taken.contains_key(&local) {
        return Err(format!("a server named {} already exists", local));
    }

    let idx: usize = option_id.split(':').nth(1).and_then(|i| i.parse().ok()).unwrap_or(0);
    let mut b = Builder::new(&local, &req.values);
    let mut cfg = match kind.as_str() {
        "remote" => b.remote(&server["remotes"][idx])?,
        "mcpb" => {
            let (dir, manifest) = prepare_bundle(&server["packages"][idx], &local).await?;
            b.bundle(&dir, &manifest)?
        }
        _ => b.package(&server["packages"][idx])?,
    };
    if req.by == "agent" && b.inputs.iter().any(|i| i["secret"] == true && req.values.contains_key(i["key"].as_str().unwrap_or(""))) {
        return Err("an agent cannot provide secrets: the user enters them in the McpHub dashboard (Library tab)".into());
    }
    if !b.needs.is_empty() {
        let needs: Vec<Value> = b.inputs.iter().filter(|i| b.needs.iter().any(|n| i["key"] == n.as_str())).cloned().collect();
        return Ok(json!({
            "ok": false,
            "name": local,
            "option": option_id,
            "needs": needs,
            "inputs": b.inputs,
            "message": format!("{} value(s) required before installing", needs.len()),
        }));
    }
    let mut stored = Vec::new();
    for (name, value) in &b.secrets {
        store_secret(name, value).map_err(|e| format!("cannot store secret {}: {}", name, e))?;
        stored.push(name.clone());
    }

    // Protections héritées : bac à sable bouclier pour le code local (sauf réglage global, qui
    // s'applique alors), aucun pour Docker (déjà isolé dans son conteneur) ni pour le distant.
    let global_sandbox = config.pointer("/settings/sandbox").filter(|v| !matches!(v, Value::Bool(false)));
    let sandbox = match kind.as_str() {
        "remote" => None,
        "oci" => Some(json!(false)),
        _ if global_sandbox.is_some() => None,
        _ => Some(json!("shield")),
    };
    if let Some(sb) = &sandbox {
        cfg["sandbox"] = sb.clone();
    }
    if cfg.get("env").and_then(|e| e.as_object()).map_or(false, |e| e.is_empty()) {
        cfg.as_object_mut().map(|o| o.remove("env"));
    }
    cfg["registry"] = json!({
        "name": req.name,
        "version": s(&server, "version"),
        "install": kind,
        "installedAt": crate::audit::iso_utc_now(),
        "by": req.by,
    });
    config[key][&local] = cfg.clone();
    write_hub_config(&config)?;

    let oauth = kind == "remote" && cfg["auth"] == "oauth";
    let shown = crate::redaction::redact_json_value(&cfg);
    eprintln!("[McpHub][LIBRARY] {} installe depuis le registre ({}, {}, par {})", local, req.name, kind, req.by);
    Ok(json!({
        "ok": true,
        "name": local,
        "registryName": req.name,
        "option": option_id,
        "kind": kind,
        "oauth": oauth,
        "secretsStored": stored,
        "sandbox": sandbox.unwrap_or(Value::Null),
        "config": shown,
    }))
}

/// Retire un serveur installé depuis le registre (et son bundle éventuel). Les secrets restent
/// au Trousseau : ils peuvent servir à d'autres serveurs.
pub fn uninstall(local: &str) -> Result<bool, String> {
    let mut config = read_hub_config()?;
    let key = servers_key(&config);
    let removed = config.get_mut(key).and_then(|v| v.as_object_mut()).and_then(|o| o.remove(local)).is_some();
    if removed {
        write_hub_config(&config)?;
        let dir = servers_dir().join(local);
        if dir.starts_with(servers_dir()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    Ok(removed)
}

// ─── Réglages ────────────────────────────────────────────────

/// settings.registry.agentInstall : "trusted" (défaut, score >= minTrust), "any" ou "off".
fn agent_policy() -> (String, i64) {
    let config = read_hub_config().unwrap_or_else(|_| json!({}));
    let r = config.pointer("/settings/registry");
    let mode = r
        .and_then(|r| r.get("agentInstall"))
        .and_then(|v| v.as_str())
        .unwrap_or("trusted")
        .to_ascii_lowercase();
    let min = r.and_then(|r| r.get("minTrust")).and_then(|v| v.as_i64()).unwrap_or(60);
    (mode, min)
}

// ─── Appels de l'agent (mcphub_call, server = "mcphub") ──────

/// Outils internes accessibles par `mcphub_call` avec `server: "mcphub"`, sans aucune
/// définition d'outil supplémentaire dans le contexte de l'agent.
pub async fn agent_call(tool: &str, args: &Value) -> Result<Value, String> {
    match tool {
        "registry_search" | "search" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
            let res = search(q, None, limit.clamp(1, 20)).await?;
            let items: Vec<Value> = res["servers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| {
                    let desc: String = x["description"].as_str().unwrap_or("").chars().take(110).collect();
                    json!({
                        "name": x["name"],
                        "description": desc,
                        "trust": x["trust"]["score"],
                        "kinds": x["kinds"],
                        "installed": x["installed"],
                    })
                })
                .collect();
            Ok(json!({
                "servers": items,
                "next": "mcphub_call server=mcphub tool=registry_install arguments={name}. Trust is partial until install checks the repository.",
            }))
        }
        "registry_info" | "info" => {
            let name = args.get("name").and_then(|v| v.as_str()).ok_or("name required")?;
            let d = details(name).await?;
            Ok(json!({
                "name": d["name"], "description": d["description"], "version": d["version"],
                "repository": d["repository"], "trust": d["trust"],
                "options": d["options"].as_array().into_iter().flatten().map(|o| json!({
                    "id": o["id"], "label": o["label"], "available": o["available"], "reason": o["reason"],
                    "inputs": o["inputs"].as_array().into_iter().flatten().map(|i| json!({"key": i["key"], "secret": i["secret"], "required": i["required"]})).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }))
        }
        "registry_install" | "install" => {
            let name = args.get("name").and_then(|v| v.as_str()).ok_or("name required")?;
            let (mode, min) = agent_policy();
            if mode == "off" {
                return Err("agent installs are turned off (settings.registry.agentInstall = \"off\"): ask the user to add it from the McpHub dashboard, Library tab".into());
            }
            if mode != "any" {
                let d = details(name).await?;
                let score = d["trust"]["score"].as_i64().unwrap_or(0);
                if score < min {
                    return Err(format!(
                        "trust score {} is below {} for agent installs: ask the user to review it in the McpHub dashboard (Library tab)",
                        score, min
                    ));
                }
            }
            let values = args.get("values").and_then(|v| v.as_object()).cloned().unwrap_or_default();
            install(InstallRequest {
                name: name.to_string(),
                option: args.get("option").and_then(|v| v.as_str()).map(String::from),
                values,
                local_name: args.get("server_name").and_then(|v| v.as_str()).map(String::from),
                by: "agent".into(),
            })
            .await
        }
        other => Err(format!(
            "unknown McpHub tool {}: use registry_search, registry_info or registry_install",
            other
        )),
    }
}

// ─── CLI : McpHub registry ... ───────────────────────────────

fn prompt_line(label: &str) -> String {
    use std::io::{BufRead, Write};
    eprint!("{} : ", label);
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim_end_matches(['\n', '\r']).to_string()
}

pub async fn run_cli(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("help");
    match sub {
        "search" | "find" => {
            let q = args[1..].join(" ");
            match search(&q, None, 30).await {
                Ok(res) => {
                    if res["stale"] == true {
                        eprintln!("(hors ligne : resultats en cache)");
                    }
                    for x in res["servers"].as_array().into_iter().flatten() {
                        let desc: String = x["description"].as_str().unwrap_or("").chars().take(70).collect();
                        let kinds: Vec<&str> = x["kinds"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).collect();
                        println!(
                            "{:3}  {:52} {:14} {}{}",
                            x["trust"]["score"].as_i64().unwrap_or(0),
                            x["name"].as_str().unwrap_or(""),
                            kinds.join(","),
                            desc,
                            if x["installed"].is_string() { "  [installe]" } else { "" }
                        );
                    }
                    0
                }
                Err(e) => {
                    eprintln!("Registre : {}", e);
                    1
                }
            }
        }
        "info" | "show" => {
            let Some(name) = args.get(1) else {
                eprintln!("Usage : McpHub registry info NOM");
                return 2;
            };
            match details(name).await {
                Ok(d) => {
                    println!("{}  v{}", d["name"].as_str().unwrap_or(""), d["version"].as_str().unwrap_or(""));
                    println!("{}", d["description"].as_str().unwrap_or(""));
                    if let Some(r) = d["repository"].as_str() {
                        println!("Code : {}", r);
                    }
                    println!("\nConfiance : {}/100 ({})", d["trust"]["score"], d["trust"]["level"].as_str().unwrap_or(""));
                    for sig in d["trust"]["signals"].as_array().into_iter().flatten() {
                        println!("  {:+4}  {}", sig["points"].as_i64().unwrap_or(0), sig["label"].as_str().unwrap_or(""));
                    }
                    for w in d["trust"]["warnings"].as_array().into_iter().flatten() {
                        println!("  !     {}", w.as_str().unwrap_or(""));
                    }
                    for c in d["trust"]["capabilities"].as_array().into_iter().flatten() {
                        println!("  i     {}", c.as_str().unwrap_or(""));
                    }
                    println!("\nInstallation :");
                    for o in d["options"].as_array().into_iter().flatten() {
                        println!(
                            "  {:10} {}{}",
                            o["id"].as_str().unwrap_or(""),
                            o["label"].as_str().unwrap_or(""),
                            o["reason"].as_str().map(|r| format!("  (indisponible : {})", r)).unwrap_or_default()
                        );
                    }
                    0
                }
                Err(e) => {
                    eprintln!("Registre : {}", e);
                    1
                }
            }
        }
        "install" | "add" => {
            let mut name = None;
            let mut option = None;
            let mut local = None;
            let mut values = Map::new();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--option" => option = it.next().cloned(),
                    "--name" => local = it.next().cloned(),
                    kv if kv.contains('=') && name.is_some() => {
                        let (k, v) = kv.split_once('=').unwrap();
                        values.insert(k.to_string(), json!(v));
                    }
                    n => name = Some(n.to_string()),
                }
            }
            let Some(name) = name else {
                eprintln!("Usage : McpHub registry install NOM [--option ID] [--name NOM_LOCAL] [CLE=VALEUR...]");
                return 2;
            };
            let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
            for _ in 0..4 {
                let req = InstallRequest {
                    name: name.clone(),
                    option: option.clone(),
                    values: values.clone(),
                    local_name: local.clone(),
                    by: "cli".into(),
                };
                match install(req).await {
                    Ok(r) if r["ok"] == true => {
                        println!("{} installe ({}).", r["name"].as_str().unwrap_or(""), r["kind"].as_str().unwrap_or(""));
                        if r["oauth"] == true {
                            println!("Connexion OAuth au premier appel, ou maintenant : McpHub oauth login {}", r["name"].as_str().unwrap_or(""));
                        }
                        return 0;
                    }
                    Ok(r) => {
                        option = r["option"].as_str().map(String::from);
                        if !interactive {
                            eprintln!("Valeurs requises :");
                            for n in r["needs"].as_array().into_iter().flatten() {
                                eprintln!("  {}  {}", n["key"].as_str().unwrap_or(""), n["description"].as_str().unwrap_or(""));
                            }
                            eprintln!("Relancer avec CLE=VALEUR, ou dans un terminal pour les saisir.");
                            return 1;
                        }
                        for n in r["needs"].as_array().into_iter().flatten() {
                            let key = n["key"].as_str().unwrap_or("").to_string();
                            let label = format!("{} ({})", n["label"].as_str().unwrap_or(&key), n["description"].as_str().unwrap_or(""));
                            let v = if n["secret"] == true {
                                crate::vault::read_secret_from_stdin(&label).unwrap_or_default()
                            } else {
                                prompt_line(&label)
                            };
                            values.insert(key, json!(v));
                        }
                    }
                    Err(e) => {
                        eprintln!("Installation impossible : {}", e);
                        return 1;
                    }
                }
            }
            1
        }
        "sync" | "update" => {
            eprintln!("Telechargement du catalogue du registre (l'API est lente : quelques minutes la premiere fois)...");
            let task = tokio::spawn(async {
                match sync_popular().await {
                    Ok(n) => eprintln!("  {} serveurs populaires (Registre MCP de GitHub)", n),
                    Err(e) => eprintln!("  Registre MCP de GitHub indisponible : {}", e),
                }
                sync_catalog().await
            });
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                eprint!("\r  {} fiches", SYNC_DONE.load(std::sync::atomic::Ordering::SeqCst));
                if task.is_finished() {
                    break;
                }
            }
            eprintln!();
            match task.await {
                Ok(Ok(n)) => {
                    println!("Catalogue a jour : {} serveurs.", n);
                    0
                }
                Ok(Err(e)) => {
                    eprintln!("Interrompu (reprise au prochain lancement) : {}", e);
                    1
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        "uninstall" | "remove" | "rm" => {
            let Some(local) = args.get(1) else {
                eprintln!("Usage : McpHub registry uninstall NOM_LOCAL");
                return 2;
            };
            match uninstall(local) {
                Ok(true) => {
                    println!("{} retire.", local);
                    0
                }
                Ok(false) => {
                    eprintln!("{} : serveur inconnu", local);
                    1
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        _ => {
            println!(
                "McpHub registry : bibliotheque du registre MCP officiel\n\n  \
                 search [MOTS]      chercher (score de confiance, types d'installation)\n  \
                 info NOM           fiche, score detaille, options d'installation\n  \
                 install NOM        installer (secrets au Trousseau, bac a sable, epinglage)\n  \
                 uninstall NOM      retirer un serveur installe\n  \
                 sync               telecharger ou mettre a jour le catalogue local\n\n\
                 Reglages (config.json > settings.registry) : agentInstall = trusted | any | off, minTrust = 60"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "server": {
                "name": "io.github.acme/weather-mcp",
                "description": "Weather forecasts",
                "version": "1.2.0",
                "repository": {"url": "https://github.com/acme/weather-mcp", "source": "github"},
                "packages": [{
                    "registryType": "npm", "identifier": "@acme/weather-mcp", "version": "1.2.0",
                    "transport": {"type": "stdio"},
                    "environmentVariables": [
                        {"name": "WEATHER_API_KEY", "isSecret": true, "isRequired": true, "description": "API key"},
                        {"name": "UNITS", "default": "metric"}
                    ],
                    "packageArguments": [{"type": "named", "name": "--region", "value": "eu"}]
                }],
                "remotes": [{"type": "streamable-http", "url": "https://weather.acme.dev/mcp",
                    "headers": [{"name": "Authorization", "value": "Bearer {token}", "isSecret": true, "isRequired": true}]}]
            },
            "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "updatedAt": "2026-10-01T00:00:00Z"}}
        })
    }

    #[test]
    fn score_editeur_verifie() {
        let t = trust(&sample(), Some(&json!({"stars": 500, "pushedAt": "2026-10-20T00:00:00Z", "archived": false})));
        assert!(t["score"].as_i64().unwrap() >= 75, "{}", t);
        assert_eq!(t["level"], "high");
        assert!(t["capabilities"].to_string().contains("WEATHER_API_KEY"));
        let mut other = sample();
        other["server"]["repository"]["url"] = json!("https://github.com/someone-else/weather");
        let t2 = trust(&other, None);
        assert!(t2["score"].as_i64().unwrap() < t["score"].as_i64().unwrap());
        assert_eq!(t2["complete"], false);
    }

    #[test]
    fn docker_dangereux() {
        let mut e = sample();
        e["server"]["remotes"] = json!([]);
        e["server"]["packages"] = json!([{"registryType": "oci", "identifier": "docker.io/x/y:1",
            "transport": {"type": "stdio"},
            "runtimeArguments": [{"type": "named", "name": "--privileged"}, {"type": "named", "name": "-v", "value": "/var/run/docker.sock:/var/run/docker.sock"}]}]);
        let t = trust(&e, None);
        assert!(t["warnings"].to_string().contains("privileged"));
        assert!(t["score"].as_i64().unwrap() < 50);
    }

    #[test]
    fn construction_npm_et_secrets() {
        let e = sample();
        let mut values = Map::new();
        let mut b = Builder::new("weather", &values);
        let cfg = b.package(&e["server"]["packages"][0]).unwrap();
        assert_eq!(b.needs, vec!["env:WEATHER_API_KEY".to_string()]);
        assert_eq!(cfg["args"], json!(["-y", "@acme/weather-mcp@1.2.0", "--region", "eu"]));
        assert_eq!(cfg["env"]["UNITS"], "metric");
        values.insert("env:WEATHER_API_KEY".into(), json!("k-123"));
        let mut b = Builder::new("weather", &values);
        let cfg = b.package(&e["server"]["packages"][0]).unwrap();
        assert!(b.needs.is_empty());
        assert_eq!(cfg["env"]["WEATHER_API_KEY"], "${WEATHER_API_KEY}");
        assert_eq!(b.secrets, vec![("WEATHER_API_KEY".to_string(), "k-123".to_string())]);
    }

    #[test]
    fn construction_distant_modele() {
        let e = sample();
        let mut values = Map::new();
        values.insert("var:token".into(), json!("abc"));
        let mut b = Builder::new("weather", &values);
        let cfg = b.remote(&e["server"]["remotes"][0]).unwrap();
        assert_eq!(cfg["headers"]["Authorization"], "Bearer ${WEATHER_TOKEN}");
        assert_eq!(cfg["auth"], "none");
        assert_eq!(b.secrets[0].0, "WEATHER_TOKEN");
    }

    #[test]
    fn noms_et_dates() {
        assert_eq!(env_var_name("my-server", "api.key"), "MY_SERVER_API_KEY");
        assert_eq!(env_var_name("9x", "k"), "MCP_9X_K");
        assert_eq!(placeholders("Bearer {a} {b_c}"), vec!["a", "b_c"]);
        assert_eq!(namespace_domain("com.stripe"), "stripe.com");
        assert_eq!(local_slug("io.github.acme/weather-mcp", &Map::new()), "weather-mcp");
        let mut taken = Map::new();
        taken.insert("weather-mcp".into(), json!({}));
        assert_eq!(local_slug("io.github.acme/weather-mcp", &taken), "weather-mcp-2");
        assert_eq!(local_slug("com.acme/mcp", &Map::new()), "com-acme");
        assert!(days_since("1970-01-02T00:00:00Z").unwrap() > 20000);
        assert_eq!(github_repo("https://github.com/a/b.git"), Some(("a".into(), "b".into())));
    }
}
