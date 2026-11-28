//! Transport « tunnel » : `cloudflared` relaie `https://<domaine>` vers le port local dédié.
//!
//! `McpHub remote enable --tunnel hub.example.com` :
//! 1. vérifie `cloudflared` et la connexion au compte Cloudflare (`cloudflared tunnel login`,
//!    une page à valider dans le navigateur la première fois) ;
//! 2. crée (ou reprend) le tunnel nommé `mcphub` (`mcphub-<instance>` pour une instance) ;
//! 3. crée l'enregistrement DNS du domaine, sans jamais écraser un enregistrement existant ;
//! 4. écrit `~/.McpHub/remote/cloudflared.yml` (le domaine vers 127.0.0.1:<port>, tout le
//!    reste en 404) ;
//! 5. installe un service de fond (LaunchAgent sur macOS, unité systemd utilisateur sur Linux)
//!    qui relance le tunnel au démarrage de la session.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{save_settings, Settings, Transport};

fn cloudflared() -> Result<PathBuf, String> {
    let candidates = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    for dir in candidates {
        let path = dir.join("cloudflared");
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(if cfg!(target_os = "macos") {
        "cloudflared introuvable : brew install cloudflared".into()
    } else {
        "cloudflared introuvable : https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/".into()
    })
}

fn tunnel_name() -> String {
    match &crate::instance::current().name {
        Some(name) => format!("mcphub-{}", name),
        None => "mcphub".into(),
    }
}

fn valid_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    domain.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn run(bin: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("cloudflared {} : {}", args.join(" "), e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "cloudflared {} a echoue : {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn find_tunnel(bin: &Path, name: &str) -> Result<Option<String>, String> {
    let listed = run(bin, &["tunnel", "list", "--output", "json"])?;
    let tunnels: serde_json::Value = serde_json::from_str(listed.trim())
        .map_err(|e| format!("liste des tunnels illisible : {}", e))?;
    Ok(tunnels.as_array().and_then(|a| {
        a.iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|t| t.get("id").and_then(|i| i.as_str()).map(String::from))
    }))
}

fn cloudflared_dir() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .ok_or("dossier personnel introuvable")?
        .join(".cloudflared"))
}

fn config_file() -> PathBuf {
    super::remote_dir().join("cloudflared.yml")
}

fn log_file() -> PathBuf {
    super::remote_dir().join("tunnel.log")
}

pub fn enable(domain: &str) -> Result<(), String> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = domain;
        return Err("tunnel : macOS et Linux seulement dans cette version".into());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if !valid_domain(&domain) {
            return Err(format!("domaine invalide : {}", domain));
        }
        let bin = cloudflared()?;
        let cf_dir = cloudflared_dir()?;
        if !cf_dir.join("cert.pem").is_file() {
            println!(
                "Connexion a Cloudflare : choisissez la zone de {} dans la page qui s'ouvre.",
                domain
            );
            let status = Command::new(&bin)
                .args(["tunnel", "login"])
                .status()
                .map_err(|e| format!("cloudflared tunnel login : {}", e))?;
            if !status.success() || !cf_dir.join("cert.pem").is_file() {
                return Err(
                    "connexion a Cloudflare non terminee (cloudflared tunnel login)".into(),
                );
            }
        }

        let name = tunnel_name();
        let id = match find_tunnel(&bin, &name)? {
            Some(id) => id,
            None => {
                run(&bin, &["tunnel", "create", &name])?;
                find_tunnel(&bin, &name)?
                    .ok_or_else(|| format!("tunnel {} cree mais introuvable", name))?
            }
        };
        let credentials = cf_dir.join(format!("{}.json", id));
        if !credentials.is_file() {
            return Err(format!(
                "identifiants du tunnel absents ({}) : supprimez le tunnel {} dans Cloudflare puis relancez",
                credentials.display(),
                name
            ));
        }

        // Jamais --overwrite-dns : un enregistrement existant du domaine reste intact.
        if let Err(e) = run(&bin, &["tunnel", "route", "dns", &name, &domain]) {
            if !e.contains("already") {
                return Err(e);
            }
            println!(
                "Enregistrement DNS deja present pour {} (garde tel quel).",
                domain
            );
        }

        let settings = Settings {
            enabled: true,
            transport: Transport::Tunnel,
            public_url: format!("https://{}", domain),
            port: Settings::load()
                .map(|s| s.port)
                .unwrap_or_else(|_| Settings::default_port()),
            relay_url: None,
        };
        super::ensure_private_dir()?;
        let yaml = format!(
            "tunnel: {id}\ncredentials-file: {creds}\nno-autoupdate: true\ningress:\n  - hostname: {domain}\n    service: http://127.0.0.1:{port}\n  - service: http_status:404\n",
            id = id,
            creds = credentials.display(),
            domain = domain,
            port = settings.port,
        );
        std::fs::write(config_file(), yaml)
            .map_err(|e| format!("{} : {}", config_file().display(), e))?;
        install_service(&bin)?;
        save_settings(&settings)
    }
}

#[cfg(target_os = "macos")]
fn label() -> String {
    format!("{}.tunnel", crate::instance::current().launchd_label())
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .ok_or("dossier personnel introuvable")?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label())))
}

#[cfg(target_os = "linux")]
fn unit_name() -> String {
    crate::instance::current()
        .systemd_unit()
        .replace(".service", "-tunnel.service")
}

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .ok_or("dossier personnel introuvable")?
        .join(".config/systemd/user")
        .join(unit_name()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn install_service(bin: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let path = plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>tunnel</string>
        <string>--config</string>
        <string>{config}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
</dict>
</plist>"#,
            label = label(),
            bin = xml_escape(&bin.display().to_string()),
            config = xml_escape(&config_file().display().to_string()),
            log = xml_escape(&log_file().display().to_string()),
        );
        std::fs::write(&path, plist).map_err(|e| format!("{} : {}", path.display(), e))?;
        let path_str = path.display().to_string();
        let _ = Command::new("launchctl")
            .args(["unload", &path_str])
            .output();
        let out = Command::new("launchctl")
            .args(["load", &path_str])
            .output()
            .map_err(|e| format!("launchctl : {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "launchctl load : {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let unit = format!(
            "[Unit]\nDescription=McpHub remote access tunnel\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={bin} tunnel --config {config} run\nRestart=always\nRestartSec=5\nStandardError=append:{log}\n\n[Install]\nWantedBy=default.target\n",
            bin = bin.display(),
            config = config_file().display(),
            log = log_file().display(),
        );
        std::fs::write(&path, unit).map_err(|e| format!("{} : {}", path.display(), e))?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let out = Command::new("systemctl")
            .args(["--user", "enable", "--now", &unit_name()])
            .output()
            .map_err(|e| format!("systemctl : {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "systemctl enable : {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = bin;
        Err("tunnel : macOS et Linux seulement dans cette version".into())
    }
}

/// Arrête et retire le service du tunnel (le tunnel et le DNS restent dans Cloudflare).
pub fn remove_service() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let path = plist_path()?;
        if path.exists() {
            let _ = Command::new("launchctl")
                .args(["unload", &path.display().to_string()])
                .output();
            std::fs::remove_file(&path).map_err(|e| format!("{} : {}", path.display(), e))?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        if path.exists() {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", &unit_name()])
                .output();
            std::fs::remove_file(&path).map_err(|e| format!("{} : {}", path.display(), e))?;
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
        }
    }
    Ok(())
}

/// État lisible du service du tunnel.
pub fn service_state() -> String {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("launchctl").args(["list", &label()]).output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                if text.contains("\"PID\"") {
                    "en marche".into()
                } else {
                    "charge, arrete (voir ~/.McpHub/remote/tunnel.log)".into()
                }
            }
            _ => "absent".into(),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match Command::new("systemctl")
            .args(["--user", "is-active", &unit_name()])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => "inconnu".into(),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "non pris en charge".into()
    }
}

/// Relance le démon pour qu'il applique les réglages (port du tunnel ou relais).
pub fn restart_daemon() {
    #[cfg(target_os = "macos")]
    {
        let label = crate::instance::current().launchd_label();
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{}/{}", uid, label);
        match Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .output()
        {
            Ok(o) if o.status.success() => println!("Demon McpHub relance."),
            _ => println!("Relancez le demon McpHub pour appliquer (McpHub install)."),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let unit = crate::instance::current().systemd_unit();
        match Command::new("systemctl")
            .args(["--user", "restart", &unit])
            .output()
        {
            Ok(o) if o.status.success() => println!("Demon McpHub relance."),
            _ => println!("Relancez le demon McpHub pour appliquer (McpHub install)."),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("Relancez McpHub pour appliquer les reglages.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains() {
        assert!(valid_domain("hub.example.com"));
        assert!(valid_domain("a-b.example.co.uk"));
        assert!(!valid_domain("localhost"));
        assert!(!valid_domain("-bad.example.com"));
        assert!(!valid_domain("hub..example.com"));
        assert!(!valid_domain("hub.example.com/path"));
        assert!(!valid_domain("hub example.com"));
    }
}
