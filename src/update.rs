use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const REPO: &str = "Soflutionltd/McpHub";

pub fn run() {
    if let Err(e) = update() {
        eprintln!("✗ Update failed: {}", e);
        std::process::exit(1);
    }
}

fn update() -> Result<(), String> {
    let current_version = env!("CARGO_PKG_VERSION");
    println!("Checking for updates (current: v{})...", current_version);

    let latest_version = latest_release_version()?;
    if !is_newer(&latest_version, current_version) {
        println!("McpHub is up to date (v{}).", current_version);
        return Ok(());
    }
    println!("New version available: v{}! Downloading...", latest_version);

    let asset_name = asset_for(env::consts::OS, env::consts::ARCH).ok_or_else(|| {
        format!(
            "no pre-built binary for {}/{}",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;
    let base = format!("https://github.com/{}/releases/download/v{}", REPO, latest_version);

    let work = PrivateDir::create()?;
    let archive_path = work.path.join(asset_name);
    let checksum_path = work.path.join(format!("{}.sha256", asset_name));
    download(&format!("{}/{}", base, asset_name), &archive_path)?;
    download(&format!("{}/{}.sha256", base, asset_name), &checksum_path)
        .map_err(|e| format!("no checksum published for {} ({}): refusing to install an unverified binary", asset_name, e))?;
    verify_sha256(&archive_path, &checksum_path)?;

    println!("Extracting...");
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&work.path)
        .status()
        .map_err(|e| format!("tar unavailable: {}", e))?;
    if !status.success() {
        return Err("extraction failed".into());
    }
    let binary_name = if cfg!(windows) { "McpHub.exe" } else { "McpHub" };
    let new_binary = work.path.join(binary_name);
    if !new_binary.is_file() {
        return Err(format!("extracted binary not found at {}", new_binary.display()));
    }
    #[cfg(unix)]
    set_executable(&new_binary)?;
    if !reports_version(&new_binary, &latest_version) {
        return Err("the downloaded binary does not start or reports another version".into());
    }

    println!("Installing new binary...");
    let current_exe = env::current_exe().map_err(|e| format!("current executable: {}", e))?;
    let old_exe = current_exe.with_extension("old");
    let _ = fs::remove_file(&old_exe);
    // Renommer d'abord : un binaire en cours d'exécution ne peut pas être écrasé partout.
    fs::rename(&current_exe, &old_exe).map_err(|e| format!("cannot move the current binary: {}", e))?;
    let installed = fs::copy(&new_binary, &current_exe)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            #[cfg(unix)]
            set_executable(&current_exe)?;
            if reports_version(&current_exe, &latest_version) {
                Ok(())
            } else {
                Err("the installed binary does not start".into())
            }
        });
    if let Err(e) = installed {
        let _ = fs::remove_file(&current_exe);
        fs::rename(&old_exe, &current_exe)
            .map_err(|r| format!("{} ; rollback failed too: {} (previous binary kept at {})", e, r, old_exe.display()))?;
        return Err(format!("{} ; previous version restored", e));
    }
    let _ = fs::remove_file(&old_exe);
    println!("Successfully updated to v{}!", latest_version);

    if crate::install::is_installed() {
        println!("Restarting daemon to apply changes...");
        crate::install::install();
    } else {
        println!("Restart `McpHub serve` to use the new version.");
    }
    println!("Update complete.");
    Ok(())
}

fn latest_release_version() -> Result<String, String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{}/releases/latest", REPO),
        ])
        .output()
        .map_err(|e| format!("curl unavailable: {}", e))?;
    if !output.status.success() {
        return Err("cannot reach the GitHub releases API".into());
    }
    let release: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("unreadable GitHub answer: {}", e))?;
    release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(|tag| tag.trim_start_matches('v').to_string())
        .ok_or_else(|| "no release found on GitHub".into())
}

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

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("curl unavailable: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("download failed: {}", url))
    }
}

fn verify_sha256(file: &Path, checksum_file: &Path) -> Result<(), String> {
    let expected = fs::read_to_string(checksum_file)
        .map_err(|e| e.to_string())?
        .split_whitespace()
        .next()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("malformed checksum file")?;
    let bytes = fs::read(file).map_err(|e| e.to_string())?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual == expected {
        println!("Checksum verified.");
        Ok(())
    } else {
        Err(format!("checksum mismatch (expected {}, got {})", expected, actual))
    }
}

fn reports_version(binary: &Path, version: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(version))
        .unwrap_or(false)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// Dossier temporaire propre à cette mise à jour (création exclusive, droits 700) : un autre
/// utilisateur de la machine ne peut pas y déposer l'archive ou le binaire à notre place.
struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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

    #[test]
    fn jamais_de_retour_en_arriere() {
        assert!(is_newer("5.10.0", "5.9.1"));
        assert!(is_newer("6.0.0", "5.9.1"));
        assert!(!is_newer("5.9.1", "5.9.1"));
        assert!(!is_newer("5.9.0", "5.9.1"));
        assert!(!is_newer("garbage", "5.9.1"));
    }

    #[test]
    fn empreinte_verifiee() {
        let dir = PrivateDir::create().unwrap();
        let file = dir.path.join("a.tar.gz");
        fs::write(&file, b"hello").unwrap();
        let sum = dir.path.join("a.tar.gz.sha256");
        fs::write(&sum, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  a.tar.gz\n").unwrap();
        assert!(verify_sha256(&file, &sum).is_ok());
        fs::write(&sum, "0000000000000000000000000000000000000000000000000000000000000000  a.tar.gz\n").unwrap();
        assert!(verify_sha256(&file, &sum).is_err());
        fs::write(&sum, "not a checksum").unwrap();
        assert!(verify_sha256(&file, &sum).is_err());
    }

    #[test]
    fn macos_toujours_universel() {
        assert_eq!(asset_for("macos", "aarch64"), Some("McpHub-macos-universal.tar.gz"));
        assert_eq!(asset_for("macos", "x86_64"), Some("McpHub-macos-universal.tar.gz"));
    }
}
