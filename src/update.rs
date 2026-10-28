//! Mises à jour depuis les releases GitHub.
//!
//! Chaque release publie `latest.json` (version, empreinte SHA-256 de chaque fichier, déploiement
//! progressif, pause) signé avec minisign. La clé privée n'existe que dans 1Password et dans
//! l'environnement `release` de GitHub Actions ; la clé publique est compilée ici. Un fichier dont
//! l'empreinte n'est pas dans un manifeste signé n'est jamais installé.
//!
//! Le démon principal vérifie au démarrage puis toutes les 6 heures, installe la nouvelle version
//! à côté de l'ancienne (`McpHub.old`) et chaque instance redémarre sur le nouveau binaire dès
//! qu'aucune requête n'est en cours. Si la nouvelle version plante au démarrage, l'ancienne est
//! remise en place et la version fautive n'est plus proposée automatiquement.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPO: &str = "Soflutionltd/McpHub";
const UPDATE_PUBLIC_KEY: &str = "RWRD5ojsqnn9eHCnHVV//elN1FbXq0QpiQZ/PhpAahl0JCdH0z0VjR4Q";
const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CLIENT_ASSET: &str = "mcphub-client.mjs";

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_ASSET_BYTES: usize = 200 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Démarrages de la nouvelle version tolérés avant de revenir à l'ancienne.
const MAX_PROBATION_BOOTS: u32 = 3;
/// Durée de fonctionnement après laquelle la nouvelle version est jugée saine.
const PROBATION: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub rollout: Rollout,
    #[serde(default)]
    pub paused: bool,
    pub assets: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Rollout {
    /// Part des installations servies avant `full_at` (0 à 100).
    pub percent: u8,
    /// Instant (secondes Unix) à partir duquel tout le monde est servi.
    pub full_at: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Trigger {
    Manual,
    Automatic,
}

#[derive(Debug)]
pub enum Outcome {
    UpToDate,
    Paused(String),
    /// Hors de la part servie pour l'instant ; servie au plus tard à `full_at`.
    NotYet { version: String, full_at: u64 },
    /// Version revenue en arrière sur cette machine après un plantage.
    Rejected(String),
    Installed { version: String, client_error: Option<String> },
}

/// `McpHub update [--check]`.
pub async fn run(check_only: bool) {
    match run_inner(check_only).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("✗ Update failed: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run_inner(check_only: bool) -> Result<(), String> {
    println!("Checking for updates (current: v{})...", VERSION);
    if check_only {
        let manifest = fetch_manifest().await?;
        if is_newer(&manifest.version, VERSION) {
            println!("v{} is available. Run `McpHub update` to install it.", manifest.version);
        } else {
            println!("McpHub is up to date (v{}).", VERSION);
        }
        return Ok(());
    }
    match check_and_install(Trigger::Manual).await? {
        Outcome::UpToDate => println!("McpHub is up to date (v{}).", VERSION),
        Outcome::Paused(v) => println!("The rollout of v{} is paused, nothing installed.", v),
        Outcome::NotYet { version, .. } => println!("v{} is not served to this machine yet.", version),
        Outcome::Rejected(v) => println!("v{} was rolled back on this machine, skipped.", v),
        Outcome::Installed { version, client_error } => {
            println!("Updated to v{}.", version);
            if let Some(e) = client_error {
                println!("! Node client not updated: {}", e);
            }
            if crate::install::is_installed() {
                println!("The daemon switches to v{} as soon as it is idle.", version);
            } else {
                println!("Restart `McpHub serve` to use the new version.");
            }
        }
    }
    Ok(())
}

pub async fn check_and_install(trigger: Trigger) -> Result<Outcome, String> {
    let _lock = UpdateLock::acquire()?;
    let manifest = fetch_manifest().await?;
    if !is_newer(&manifest.version, VERSION) {
        return Ok(Outcome::UpToDate);
    }
    if manifest.paused {
        return Ok(Outcome::Paused(manifest.version));
    }
    let exe = env::current_exe().map_err(|e| format!("current executable: {}", e))?;
    if trigger == Trigger::Automatic {
        if read_state(&exe).rejected.contains(&manifest.version) {
            return Ok(Outcome::Rejected(manifest.version));
        }
        if !rollout_allows(&manifest, install_bucket(), unix_now()) {
            return Ok(Outcome::NotYet {
                version: manifest.version,
                full_at: manifest.rollout.full_at,
            });
        }
    }

    let asset = asset_for(env::consts::OS, env::consts::ARCH)
        .ok_or_else(|| format!("no pre-built binary for {}/{}", env::consts::OS, env::consts::ARCH))?;
    let archive = download_verified(&manifest, asset).await?;

    let work = PrivateDir::create()?;
    let archive_path = work.path.join(asset);
    fs::write(&archive_path, &archive).map_err(|e| e.to_string())?;
    let new_binary = extract(&work.path, &archive_path).await?;
    #[cfg(target_os = "macos")]
    verify_developer_id(&new_binary)?;
    if !reports_version(&new_binary, &manifest.version) {
        return Err("the downloaded binary does not start or reports another version".into());
    }
    install_binary(&exe, &new_binary, &manifest.version)?;

    let client_error = if manifest.assets.contains_key(CLIENT_ASSET) {
        install_client(&manifest).await.err()
    } else {
        None
    };
    Ok(Outcome::Installed {
        version: manifest.version,
        client_error,
    })
}

// ---------------------------------------------------------------------------
// Manifeste signé
// ---------------------------------------------------------------------------

async fn fetch_manifest() -> Result<Manifest, String> {
    let base = format!("https://github.com/{}/releases/latest/download", REPO);
    let body = fetch(&format!("{}/latest.json", base), MAX_MANIFEST_BYTES).await?;
    let signature = fetch(&format!("{}/latest.json.minisig", base), MAX_MANIFEST_BYTES).await?;
    let signature = String::from_utf8(signature).map_err(|_| "unreadable manifest signature")?;
    verify_manifest(&body, &signature, UPDATE_PUBLIC_KEY)
}

pub fn verify_manifest(body: &[u8], signature: &str, public_key: &str) -> Result<Manifest, String> {
    let key = minisign_verify::PublicKey::from_base64(public_key)
        .map_err(|e| format!("invalid update key: {}", e))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| format!("malformed manifest signature: {}", e))?;
    key.verify(body, &signature, false)
        .map_err(|e| format!("manifest signature rejected: {}", e))?;
    let manifest: Manifest =
        serde_json::from_slice(body).map_err(|e| format!("malformed manifest: {}", e))?;
    if parse_version(&manifest.version).is_none() || manifest.rollout.percent > 100 {
        return Err("malformed manifest: version or rollout".into());
    }
    Ok(manifest)
}

fn rollout_allows(manifest: &Manifest, bucket: u8, now: u64) -> bool {
    now >= manifest.rollout.full_at || bucket < manifest.rollout.percent
}

/// Seau stable de 0 à 99 tiré d'un identifiant aléatoire local, jamais transmis.
fn install_bucket() -> u8 {
    let path = crate::instance::hub_dir().join("install-id");
    let id = fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let id = crate::http_guard::random_token().unwrap_or_else(|| {
                format!("{}-{}-{}", unix_now(), std::process::id(), path.display())
            });
            let _ = fs::write(&path, &id);
            id
        });
    bucket_of(&id)
}

fn bucket_of(id: &str) -> u8 {
    let digest = Sha256::digest(id.as_bytes());
    (u16::from_be_bytes([digest[0], digest[1]]) % 100) as u8
}

// ---------------------------------------------------------------------------
// Téléchargement et installation
// ---------------------------------------------------------------------------

async fn fetch(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent(format!("McpHub/{}", VERSION))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download failed ({}): {}", url, e))?;
    if response.content_length().is_some_and(|len| len > limit as u64) {
        return Err(format!("{} is larger than expected", url));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("download failed ({}): {}", url, e))? {
        if body.len() + chunk.len() > limit {
            return Err(format!("{} is larger than expected", url));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn download_verified(manifest: &Manifest, asset: &str) -> Result<Vec<u8>, String> {
    let expected = manifest
        .assets
        .get(asset)
        .ok_or_else(|| format!("{} is not listed in the signed manifest", asset))?;
    let url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        REPO, manifest.version, asset
    );
    let bytes = fetch(&url, MAX_ASSET_BYTES).await?;
    check_sha256(&bytes, expected)?;
    Ok(bytes)
}

fn check_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("checksum mismatch (expected {}, got {})", expected, actual))
    }
}

async fn extract(dir: &Path, archive: &Path) -> Result<PathBuf, String> {
    let status = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .status()
        .await
        .map_err(|e| format!("tar unavailable: {}", e))?;
    if !status.success() {
        return Err("extraction failed".into());
    }
    let binary = dir.join(if cfg!(windows) { "McpHub.exe" } else { "McpHub" });
    if !binary.is_file() {
        return Err(format!("extracted binary not found at {}", binary.display()));
    }
    #[cfg(unix)]
    set_executable(&binary)?;
    Ok(binary)
}

/// Signé par Soflution (Developer ID, équipe TB8CC687M3) : sinon macOS redemanderait l'accès au
/// Trousseau à chaque version, et un binaire étranger ne passe pas.
#[cfg(target_os = "macos")]
fn verify_developer_id(binary: &Path) -> Result<(), String> {
    let requirement = r#"=identifier "com.soflution.mcphub" and anchor apple generic and certificate leaf[subject.OU] = "TB8CC687M3""#;
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "-R", requirement])
        .arg(binary)
        .output()
        .map_err(|e| format!("codesign unavailable: {}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "the downloaded binary is not signed by Soflution: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn reports_version(binary: &Path, version: &str) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).trim() == format!("McpHub v{}", version)
        })
        .unwrap_or(false)
}

/// L'ancien binaire reste en `McpHub.old` jusqu'à ce que la nouvelle version ait tourné
/// `PROBATION` sans planter.
fn install_binary(exe: &Path, new_binary: &Path, version: &str) -> Result<(), String> {
    let old = old_binary(exe);
    let staged = exe.with_extension("new");
    let _ = fs::remove_file(&staged);
    fs::copy(new_binary, &staged).map_err(|e| format!("cannot write next to {}: {}", exe.display(), e))?;
    #[cfg(unix)]
    set_executable(&staged)?;
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).map_err(|e| {
        let _ = fs::remove_file(&staged);
        format!("cannot move the current binary: {}", e)
    })?;
    if let Err(e) = fs::rename(&staged, exe) {
        let _ = fs::rename(&old, exe);
        let _ = fs::remove_file(&staged);
        return Err(format!("cannot install the new binary: {} ; previous version kept", e));
    }
    let mut state = read_state(exe);
    state.pending = Some(Pending {
        from: VERSION.to_string(),
        to: version.to_string(),
        boots: 0,
    });
    write_state(exe, &state)
}

async fn install_client(manifest: &Manifest) -> Result<(), String> {
    let bytes = download_verified(manifest, CLIENT_ASSET).await?;
    let dir = crate::instance::hub_dir().join("client");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(CLIENT_ASSET);
    let tmp = dir.join(format!("{}.tmp.{}", CLIENT_ASSET, std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

// ---------------------------------------------------------------------------
// Période d'essai et retour arrière
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct UpdateState {
    #[serde(default)]
    pending: Option<Pending>,
    /// Versions revenues en arrière sur cette machine : plus proposées automatiquement.
    #[serde(default)]
    rejected: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Pending {
    from: String,
    to: String,
    boots: u32,
}

#[derive(Debug, PartialEq)]
enum BootAction {
    Continue,
    RollBack,
}

fn old_binary(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

fn state_path(exe: &Path) -> PathBuf {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    exe.with_file_name(format!(".{}.update.json", name))
}

fn read_state(exe: &Path) -> UpdateState {
    fs::read(state_path(exe))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_state(exe: &Path, state: &UpdateState) -> Result<(), String> {
    let path = state_path(exe);
    if *state == UpdateState::default() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let text = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("{}: {}", path.display(), e))
}

fn on_boot(state: &mut UpdateState, running: &str) -> BootAction {
    let Some(pending) = state.pending.as_mut() else {
        return BootAction::Continue;
    };
    if pending.to != running {
        state.pending = None;
        return BootAction::Continue;
    }
    pending.boots += 1;
    if pending.boots > MAX_PROBATION_BOOTS {
        let rejected = pending.to.clone();
        state.pending = None;
        if !state.rejected.contains(&rejected) {
            state.rejected.push(rejected);
        }
        return BootAction::RollBack;
    }
    BootAction::Continue
}

/// À appeler au tout début de `McpHub serve`, avant de lire la config : une version qui plante
/// au démarrage y repasse à chaque relance du service.
pub fn startup_guard() {
    let Ok(exe) = env::current_exe() else { return };
    let mut state = read_state(&exe);
    let action = on_boot(&mut state, VERSION);
    if let Err(e) = write_state(&exe, &state) {
        eprintln!("[McpHub][UPDATE] {}", e);
    }
    if action == BootAction::RollBack {
        let old = old_binary(&exe);
        eprintln!(
            "[McpHub][UPDATE] v{} failed to start {} times: restoring the previous version",
            VERSION, MAX_PROBATION_BOOTS
        );
        if let Err(e) = fs::rename(&old, &exe) {
            eprintln!("[McpHub][UPDATE] cannot restore {}: {}", old.display(), e);
            return;
        }
        restart_process(&exe);
    }
}

fn confirm_healthy(exe: &Path) {
    let mut state = read_state(exe);
    if state.pending.as_ref().is_some_and(|p| p.to == VERSION) {
        let from = state.pending.take().map(|p| p.from).unwrap_or_default();
        let _ = fs::remove_file(old_binary(exe));
        if let Err(e) = write_state(exe, &state) {
            eprintln!("[McpHub][UPDATE] {}", e);
        }
        eprintln!("[McpHub][UPDATE] v{} confirmed (previous: v{})", VERSION, from);
    }
}

// ---------------------------------------------------------------------------
// Tâches du démon
// ---------------------------------------------------------------------------

/// Lance les tâches de mise à jour de `McpHub serve`.
pub fn spawn_daemon_tasks(proxy: Arc<crate::proxy::ProxyServer>) {
    let Ok(exe) = env::current_exe() else { return };
    let fingerprint = binary_fingerprint(&exe);

    let healthy_exe = exe.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PROBATION).await;
        confirm_healthy(&healthy_exe);
    });

    let watched_exe = exe.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if binary_fingerprint(&watched_exe) != fingerprint {
                restart_when_idle(&proxy, &watched_exe).await;
                return;
            }
        }
    });

    if let Some(reason) = auto_update_blocked(&exe) {
        eprintln!("[McpHub][UPDATE] automatic updates off: {}", reason);
        return;
    }
    tokio::spawn(async move {
        let jitter = Duration::from_secs(u64::from(install_bucket()) * 6);
        tokio::time::sleep(Duration::from_secs(120) + jitter).await;
        loop {
            if auto_update_enabled() {
                match check_and_install(Trigger::Automatic).await {
                    Ok(Outcome::Installed { version, client_error }) => {
                        eprintln!("[McpHub][UPDATE] v{} installed", version);
                        if let Some(e) = client_error {
                            eprintln!("[McpHub][UPDATE] Node client not updated: {}", e);
                        }
                    }
                    Ok(Outcome::NotYet { version, full_at }) => eprintln!(
                        "[McpHub][UPDATE] v{} available, served here from {} at the latest",
                        version, full_at
                    ),
                    Ok(Outcome::Paused(v)) => eprintln!("[McpHub][UPDATE] rollout of v{} paused", v),
                    Ok(Outcome::Rejected(v)) => {
                        eprintln!("[McpHub][UPDATE] v{} was rolled back on this machine, skipped", v)
                    }
                    Ok(Outcome::UpToDate) => {}
                    Err(e) => eprintln!("[McpHub][UPDATE] check failed: {}", e),
                }
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

/// Seule l'instance principale met à jour le binaire partagé, jamais un binaire de développement.
fn auto_update_blocked(exe: &Path) -> Option<&'static str> {
    if cfg!(debug_assertions) {
        return Some("debug build");
    }
    if crate::instance::current().name.is_some() {
        return Some("named instance (the main instance updates the binary)");
    }
    if exe.components().any(|c| c.as_os_str() == "target") {
        return Some("binary inside a cargo target directory");
    }
    if env::var("MCPHUB_AUTO_UPDATE").is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false")) {
        return Some("MCPHUB_AUTO_UPDATE=0");
    }
    None
}

fn auto_update_enabled() -> bool {
    match crate::config::load_dedicated() {
        crate::config::Dedicated::Valid(config) => config.auto_update,
        crate::config::Dedicated::Missing => true,
        crate::config::Dedicated::Invalid(e) => {
            eprintln!("[McpHub][UPDATE] config invalid, update check skipped: {}", e);
            false
        }
    }
}

fn binary_fingerprint(exe: &Path) -> Option<(u64, SystemTime)> {
    let meta = fs::metadata(exe).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

#[cfg(unix)]
async fn restart_when_idle(proxy: &crate::proxy::ProxyServer, exe: &Path) {
    let mut idle_checks = 0;
    while idle_checks < 2 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        idle_checks = if proxy.in_flight() == 0 { idle_checks + 1 } else { 0 };
    }
    eprintln!("[McpHub][UPDATE] new binary installed: restarting");
    proxy.shutdown().await;
    restart_process(exe);
}

/// Windows ne remplace pas un programme en cours : le nouveau binaire sert au prochain démarrage
/// de session.
#[cfg(not(unix))]
async fn restart_when_idle(_proxy: &crate::proxy::ProxyServer, exe: &Path) {
    eprintln!(
        "[McpHub][UPDATE] {} updated: it is used from the next session start",
        exe.display()
    );
}

/// `exec` du binaire dans le même processus : le service (launchd, systemd) ne voit rien.
fn restart_process(exe: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(exe).args(env::args_os().skip(1)).exec();
        eprintln!("[McpHub][UPDATE] restart failed: {}", error);
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!(
            "[McpHub][UPDATE] {} restored: it is used from the next session start",
            exe.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Utilitaires
// ---------------------------------------------------------------------------

/// macOS : binaire universel (celui que publie la release signée, comme get.sh).
fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", _) => Some("McpHub-macos-universal.tar.gz"),
        ("linux", "x86_64") => Some("McpHub-linux-amd64.tar.gz"),
        ("linux", "aarch64") => Some("McpHub-linux-arm64.tar.gz"),
        ("windows", "x86_64") => Some("McpHub-windows-amd64.zip"),
        _ => None,
    }
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// Jamais de retour en arrière : une release « latest » plus ancienne n'est pas installée.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// Une seule mise à jour à la fois (démon et `McpHub update` lancés ensemble).
struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    fn acquire() -> Result<Self, String> {
        let path = crate::instance::hub_dir().join("update.lock");
        let stale = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(15 * 60));
        if stale {
            let _ = fs::remove_file(&path);
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(|_| Self { path })
            .map_err(|_| "another update is already running".to_string())
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Dossier temporaire propre à cette mise à jour (création exclusive, droits 700) : un autre
/// utilisateur de la machine ne peut pas y déposer l'archive ou le binaire à notre place.
struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    fn create() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = env::temp_dir().join(format!("mcphub-update-{}-{:x}", std::process::id(), nanos));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
        Ok(Self { path })
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/update-manifest.json");
    const FIXTURE_SIG: &str = include_str!("../tests/fixtures/update-manifest.json.minisig");
    const FIXTURE_KEY: &str = include_str!("../tests/fixtures/update-test-key.pub");

    #[test]
    fn jamais_de_retour_en_arriere() {
        assert!(is_newer("5.10.0", "5.9.1"));
        assert!(is_newer("6.0.0", "5.9.1"));
        assert!(!is_newer("5.9.1", "5.9.1"));
        assert!(!is_newer("5.9.0", "5.9.1"));
        assert!(!is_newer("garbage", "5.9.1"));
    }

    #[test]
    fn manifeste_signe_accepte() {
        let manifest = verify_manifest(FIXTURE.as_bytes(), FIXTURE_SIG, FIXTURE_KEY.trim()).unwrap();
        assert_eq!(manifest.version, "9.0.0");
        assert_eq!(manifest.rollout.percent, 10);
        assert!(manifest.assets.contains_key("McpHub-macos-universal.tar.gz"));
        assert!(manifest.assets.contains_key(CLIENT_ASSET));
        check_sha256(b"universal", &manifest.assets["McpHub-macos-universal.tar.gz"]).unwrap();
        assert!(check_sha256(b"tampered", &manifest.assets["McpHub-macos-universal.tar.gz"]).is_err());
    }

    #[test]
    fn manifeste_modifie_ou_autre_cle_refuse() {
        let tampered = FIXTURE.replace("\"percent\": 10", "\"percent\": 100");
        assert!(verify_manifest(tampered.as_bytes(), FIXTURE_SIG, FIXTURE_KEY.trim()).is_err());
        // La vraie clé de release ne valide pas une signature de test.
        assert!(verify_manifest(FIXTURE.as_bytes(), FIXTURE_SIG, UPDATE_PUBLIC_KEY).is_err());
        assert!(verify_manifest(FIXTURE.as_bytes(), "not a signature", FIXTURE_KEY.trim()).is_err());
    }

    #[test]
    fn deploiement_progressif() {
        let manifest = verify_manifest(FIXTURE.as_bytes(), FIXTURE_SIG, FIXTURE_KEY.trim()).unwrap();
        let before = manifest.rollout.full_at - 1;
        assert!(rollout_allows(&manifest, 0, before));
        assert!(rollout_allows(&manifest, 9, before));
        assert!(!rollout_allows(&manifest, 10, before));
        assert!(!rollout_allows(&manifest, 99, before));
        assert!(rollout_allows(&manifest, 99, manifest.rollout.full_at));
        let buckets: std::collections::HashSet<u8> =
            (0..2000).map(|i| bucket_of(&format!("id-{}", i))).collect();
        assert!(buckets.iter().all(|b| *b < 100));
        assert!(buckets.len() > 90);
        assert_eq!(bucket_of("stable"), bucket_of("stable"));
    }

    #[test]
    fn version_qui_plante_au_demarrage_retiree() {
        let mut state = UpdateState {
            pending: Some(Pending { from: "5.9.1".into(), to: "5.10.0".into(), boots: 0 }),
            rejected: vec![],
        };
        for _ in 0..MAX_PROBATION_BOOTS {
            assert_eq!(on_boot(&mut state, "5.10.0"), BootAction::Continue);
        }
        assert_eq!(on_boot(&mut state, "5.10.0"), BootAction::RollBack);
        assert_eq!(state.pending, None);
        assert_eq!(state.rejected, vec!["5.10.0".to_string()]);

        let mut other = UpdateState {
            pending: Some(Pending { from: "5.9.1".into(), to: "5.10.0".into(), boots: 2 }),
            rejected: vec![],
        };
        assert_eq!(on_boot(&mut other, "5.9.1"), BootAction::Continue);
        assert_eq!(other.pending, None);
    }

    #[test]
    fn installation_garde_l_ancien_binaire() {
        let dir = PrivateDir::create().unwrap();
        let exe = dir.path.join("McpHub");
        let new_binary = dir.path.join("downloaded");
        fs::write(&exe, b"old").unwrap();
        fs::write(&new_binary, b"new").unwrap();
        install_binary(&exe, &new_binary, "9.0.0").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(old_binary(&exe)).unwrap(), b"old");
        let state = read_state(&exe);
        assert_eq!(state.pending.as_ref().map(|p| p.to.as_str()), Some("9.0.0"));
        assert_eq!(state.pending.as_ref().map(|p| p.from.as_str()), Some(VERSION));
    }

    #[test]
    fn macos_toujours_universel() {
        assert_eq!(asset_for("macos", "aarch64"), Some("McpHub-macos-universal.tar.gz"));
        assert_eq!(asset_for("macos", "x86_64"), Some("McpHub-macos-universal.tar.gz"));
    }
}
