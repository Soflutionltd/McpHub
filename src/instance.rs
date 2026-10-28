//! Instances isolées, par exemple une par client.
//!
//! `MCPHUB_INSTANCE=acme MCPHUB_PORT=24690 McpHub serve` lance un McpHub qui
//! ne partage rien avec l'instance principale : dossier `~/.McpHub-acme`
//! (configuration, jeton, caches, audit), service du Trousseau `McpHub-acme`
//! (secrets et jetons OAuth), port 24690 et LaunchAgent `com.soflution.mcphub.acme`.
//! `MCPHUB_HOME` et `MCPHUB_VAULT_SERVICE` restent prioritaires s'ils sont fixés.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_PORT: u16 = 24680;
const DEFAULT_SERVICE: &str = "McpHub";
#[cfg(any(target_os = "macos", test))]
const DEFAULT_LABEL: &str = "com.soflution.mcphub";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: Option<String>,
    pub hub_dir: PathBuf,
    pub port: u16,
    pub vault_service: String,
}

impl Instance {
    #[cfg(any(target_os = "macos", test))]
    pub fn launchd_label(&self) -> String {
        match &self.name {
            Some(name) => format!("{}.{}", DEFAULT_LABEL, name),
            None => DEFAULT_LABEL.to_string(),
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn systemd_unit(&self) -> String {
        match &self.name {
            Some(name) => format!("mcphub-{}.service", name),
            None => "mcphub.service".to_string(),
        }
    }

    /// Variables à transmettre au service lancé au démarrage de la session.
    pub fn service_env(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(name) = &self.name {
            vars.push(("MCPHUB_INSTANCE", name.clone()));
        }
        if self.port != DEFAULT_PORT {
            vars.push(("MCPHUB_PORT", self.port.to_string()));
        }
        for key in ["MCPHUB_HOME", "MCPHUB_VAULT_SERVICE"] {
            if let Ok(value) = std::env::var(key) {
                if !value.is_empty() {
                    vars.push((key, value));
                }
            }
        }
        vars
    }
}

fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Résout l'instance depuis les variables d'environnement données.
pub fn resolve(env: impl Fn(&str) -> Option<String>, home: &Path) -> Result<Instance, String> {
    let var = |key: &str| env(key).filter(|value| !value.is_empty());
    let name = var("MCPHUB_INSTANCE");
    if let Some(name) = &name {
        if !valid_name(name) {
            return Err(format!(
                "MCPHUB_INSTANCE=\"{}\" is invalid: use 1 to 32 lowercase letters, digits or dashes",
                name
            ));
        }
    }

    let port = match var("MCPHUB_PORT") {
        Some(raw) => {
            let port: u16 = raw
                .parse()
                .map_err(|_| format!("MCPHUB_PORT=\"{}\" is not a port number", raw))?;
            if port < 1024 {
                return Err(format!("MCPHUB_PORT={} is reserved: use 1024 or above", port));
            }
            port
        }
        None if name.is_some() => {
            return Err("MCPHUB_PORT is required with MCPHUB_INSTANCE (each instance needs its own port)".into())
        }
        None => DEFAULT_PORT,
    };
    if name.is_some() && port == DEFAULT_PORT {
        return Err(format!(
            "MCPHUB_PORT={} belongs to the main instance: pick another port for MCPHUB_INSTANCE",
            DEFAULT_PORT
        ));
    }

    let hub_dir = match (var("MCPHUB_HOME"), &name) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(name)) => home.join(format!(".McpHub-{}", name)),
        (None, None) => home.join(".McpHub"),
    };
    let vault_service = match (var("MCPHUB_VAULT_SERVICE"), &name) {
        (Some(service), _) => service,
        (None, Some(name)) => format!("{}-{}", DEFAULT_SERVICE, name),
        (None, None) => DEFAULT_SERVICE.to_string(),
    };

    Ok(Instance { name, hub_dir, port, vault_service })
}

fn from_process_env() -> Result<Instance, String> {
    let home = dirs::home_dir().ok_or("cannot find the home directory")?;
    resolve(|key| std::env::var(key).ok(), &home)
}

static CURRENT: OnceLock<Instance> = OnceLock::new();

/// À appeler au démarrage : une instance mal configurée arrête McpHub au lieu
/// de retomber en silence sur le dossier ou le port de l'instance principale.
pub fn init() -> Result<&'static Instance, String> {
    let instance = from_process_env()?;
    Ok(CURRENT.get_or_init(|| instance))
}

pub fn current() -> &'static Instance {
    CURRENT.get_or_init(|| match from_process_env() {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("[McpHub] {}", error);
            std::process::exit(2);
        }
    })
}

pub fn hub_dir() -> PathBuf {
    current().hub_dir.clone()
}

pub fn port() -> u16 {
    current().port
}

/// Règles SBPL qui cachent aux serveurs isolés les dossiers de McpHub :
/// `~/.McpHub`, toutes les instances `~/.McpHub-*` (y compris celles créées
/// plus tard) et le dossier de l'instance courante s'il est ailleurs.
pub fn sandbox_hub_rules(home: &str) -> Vec<String> {
    let escaped: String = home
        .chars()
        .flat_map(|c| {
            let special = "\\.^$|?*+()[]{}\"".contains(c);
            special.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect();
    let mut rules = vec![format!("(regex #\"^{}/\\.McpHub(-[^/]*)?(/|$)\")", escaped)];
    let own = hub_dir().to_string_lossy().to_string();
    if !own.starts_with(&format!("{}/.McpHub", home)) {
        rules.push(format!("(subpath \"{}\")", own.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve_with(vars: &[(&str, &str)]) -> Result<Instance, String> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        resolve(|key| map.get(key).cloned(), Path::new("/home/dev"))
    }

    #[test]
    fn main_instance_keeps_the_historical_defaults() {
        let instance = resolve_with(&[]).unwrap();
        assert_eq!(instance.name, None);
        assert_eq!(instance.hub_dir, PathBuf::from("/home/dev/.McpHub"));
        assert_eq!(instance.port, DEFAULT_PORT);
        assert_eq!(instance.vault_service, "McpHub");
        assert_eq!(instance.launchd_label(), "com.soflution.mcphub");
        assert_eq!(instance.systemd_unit(), "mcphub.service");
    }

    #[test]
    fn a_named_instance_shares_nothing_with_the_main_one() {
        let instance = resolve_with(&[("MCPHUB_INSTANCE", "acme"), ("MCPHUB_PORT", "24690")]).unwrap();
        assert_eq!(instance.hub_dir, PathBuf::from("/home/dev/.McpHub-acme"));
        assert_eq!(instance.port, 24690);
        assert_eq!(instance.vault_service, "McpHub-acme");
        assert_eq!(instance.launchd_label(), "com.soflution.mcphub.acme");
        assert_eq!(instance.systemd_unit(), "mcphub-acme.service");
    }

    #[test]
    fn explicit_home_and_service_win() {
        let instance = resolve_with(&[
            ("MCPHUB_INSTANCE", "acme"),
            ("MCPHUB_PORT", "24690"),
            ("MCPHUB_HOME", "/srv/acme"),
            ("MCPHUB_VAULT_SERVICE", "AcmeVault"),
        ])
        .unwrap();
        assert_eq!(instance.hub_dir, PathBuf::from("/srv/acme"));
        assert_eq!(instance.vault_service, "AcmeVault");
    }

    #[test]
    fn the_sandbox_hides_every_instance_folder() {
        let rules = sandbox_hub_rules("/Users/dev.name");
        assert_eq!(rules[0], r#"(regex #"^/Users/dev\.name/\.McpHub(-[^/]*)?(/|$)")"#);
    }

    #[test]
    fn misconfigured_instances_are_refused() {
        assert!(resolve_with(&[("MCPHUB_INSTANCE", "acme")]).unwrap_err().contains("MCPHUB_PORT is required"));
        assert!(resolve_with(&[("MCPHUB_INSTANCE", "acme"), ("MCPHUB_PORT", "24680")])
            .unwrap_err()
            .contains("main instance"));
        assert!(resolve_with(&[("MCPHUB_INSTANCE", "Acme Corp"), ("MCPHUB_PORT", "24690")]).is_err());
        assert!(resolve_with(&[("MCPHUB_INSTANCE", "../x"), ("MCPHUB_PORT", "24690")]).is_err());
        assert!(resolve_with(&[("MCPHUB_PORT", "80")]).is_err());
        assert!(resolve_with(&[("MCPHUB_PORT", "abc")]).is_err());
    }
}
