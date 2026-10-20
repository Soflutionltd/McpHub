//! Coffre-fort des secrets : stockage dans le Trousseau macOS (Keychain) au lieu de
//! `~/.McpHub/secrets.env` en clair.
//!
//! - Les valeurs vivent dans le Trousseau (service `McpHub`, compte = nom de la variable).
//! - Seuls les NOMS des variables sont écrits sur disque (`~/.McpHub/vault-index.json`),
//!   pour savoir quoi charger au démarrage sans parcourir tout le Trousseau.
//! - Tout passe par `/usr/bin/security` : l'élément est donc toujours lu par le même
//!   programme qui l'a créé, sans fenêtre d'autorisation au démarrage du daemon.
//! - Hors macOS, le coffre est indisponible et McpHub retombe sur `secrets.env`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_SERVICE: &str = "McpHub";

fn service() -> String {
    std::env::var("MCPHUB_VAULT_SERVICE").unwrap_or_else(|_| DEFAULT_SERVICE.to_string())
}

/// Trousseau explicite (tests, ou trousseau dédié). Par défaut : trousseau de session.
fn keychain_args() -> Vec<String> {
    std::env::var("MCPHUB_KEYCHAIN")
        .ok()
        .filter(|k| !k.is_empty())
        .into_iter()
        .collect()
}

fn mcphub_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("MCPHUB_HOME") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".McpHub"))
}

fn index_path() -> Option<PathBuf> {
    mcphub_dir().map(|d| d.join("vault-index.json"))
}

pub fn secrets_env_path() -> Option<PathBuf> {
    mcphub_dir().map(|d| d.join("secrets.env"))
}

pub fn is_available() -> bool {
    cfg!(target_os = "macos") && std::path::Path::new("/usr/bin/security").exists()
}

/// Nom de variable valide : lettre ou `_`, puis lettres, chiffres ou `_`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Index des noms (jamais les valeurs)
// ---------------------------------------------------------------------------

pub fn list_names() -> Vec<String> {
    let path = match index_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    value
        .get("keys")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| is_valid_name(s))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn save_names(names: &BTreeSet<String>) -> Result<(), String> {
    let path = index_path().ok_or("dossier ~/.McpHub introuvable")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::json!({
        "service": service(),
        "note": "Noms des secrets stockes dans le Trousseau macOS. Aucune valeur ici.",
        "keys": names.iter().collect::<Vec<_>>(),
    });
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    restrict_permissions(&tmp);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

fn add_name(name: &str) -> Result<(), String> {
    let mut names: BTreeSet<String> = list_names().into_iter().collect();
    names.insert(name.to_string());
    save_names(&names)
}

fn remove_name(name: &str) -> Result<(), String> {
    let mut names: BTreeSet<String> = list_names().into_iter().collect();
    names.remove(name);
    save_names(&names)
}

#[cfg(unix)]
pub fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn restrict_permissions(_path: &std::path::Path) {}

// ---------------------------------------------------------------------------
// Accès au Trousseau
// ---------------------------------------------------------------------------

pub fn get(name: &str) -> Result<Option<String>, String> {
    if !is_available() {
        return Err("Trousseau macOS indisponible".into());
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service(), "-a", name, "-w"])
        .args(keychain_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        // Code 44 = élément introuvable.
        return Ok(None);
    }
    let mut value = String::from_utf8_lossy(&out.stdout).to_string();
    if value.ends_with('\n') {
        value.pop();
    }
    Ok(Some(value))
}

pub fn set(name: &str, value: &str) -> Result<(), String> {
    if !is_available() {
        return Err("Trousseau macOS indisponible".into());
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    if value.is_empty() {
        return Err("valeur vide".into());
    }
    // -U : met à jour l'élément s'il existe déjà.
    let out = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            &service(),
            "-a",
            name,
            "-l",
            &format!("McpHub: {}", name),
            "-w",
            value,
        ])
        .args(keychain_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "echec d'ecriture dans le Trousseau: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    add_name(name)
}

pub fn delete(name: &str) -> Result<bool, String> {
    if !is_available() {
        return Err("Trousseau macOS indisponible".into());
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    let out = Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", &service(), "-a", name])
        .args(keychain_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    remove_name(name)?;
    Ok(out.status.success())
}

/// Charge dans l'environnement du process les secrets du coffre dont la variable
/// n'est pas déjà définie. Retourne (chargés, introuvables).
pub fn load_into_env() -> (usize, Vec<String>) {
    if !is_available() {
        return (0, Vec::new());
    }
    let mut loaded = 0;
    let mut missing = Vec::new();
    for name in list_names() {
        if std::env::var(&name).is_ok() {
            continue;
        }
        match get(&name) {
            Ok(Some(value)) => {
                std::env::set_var(&name, value);
                loaded += 1;
            }
            _ => missing.push(name),
        }
    }
    (loaded, missing)
}

// ---------------------------------------------------------------------------
// secrets.env
// ---------------------------------------------------------------------------

/// Lit un fichier KEY=VALUE (commentaires `#`, guillemets simples ou doubles retirés).
pub fn parse_env_file(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let mut v = v.trim();
            if v.len() >= 2
                && ((v.starts_with('"') && v.ends_with('"'))
                    || (v.starts_with('\'') && v.ends_with('\'')))
            {
                v = &v[1..v.len() - 1];
            }
            if is_valid_name(k) {
                out.push((k.to_string(), v.to_string()));
            }
        }
    }
    out
}

fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return format!("[{} caracteres]", n);
    }
    let tail: String = value.chars().skip(n - 4).collect();
    format!("****{} ({} caracteres)", tail, n)
}

fn read_secret_from_stdin(name: &str) -> Result<String, String> {
    let tty = atty_stdin();
    if tty {
        eprint!("Valeur de {} (saisie masquee) : ", name);
        let _ = io::stderr().flush();
        let _ = Command::new("stty")
            .arg("-echo")
            .stdin(Stdio::inherit())
            .status();
    }
    let mut line = String::new();
    let res = io::stdin().lock().read_line(&mut line);
    if tty {
        let _ = Command::new("stty")
            .arg("echo")
            .stdin(Stdio::inherit())
            .status();
        eprintln!();
    }
    res.map_err(|e| e.to_string())?;
    let value = line.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        return Err("valeur vide".into());
    }
    Ok(value)
}

fn atty_stdin() -> bool {
    Command::new("test")
        .arg("-t")
        .arg("0")
        .stdin(Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// CLI : McpHub vault ...
// ---------------------------------------------------------------------------

pub fn run_cli(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    if !is_available() && sub != "help" {
        eprintln!("Le coffre utilise le Trousseau macOS : indisponible sur ce systeme.");
        eprintln!("McpHub continue d'utiliser ~/.McpHub/secrets.env.");
        return;
    }
    match sub {
        "status" => cli_status(),
        "list" | "ls" => cli_list(),
        "set" | "add" => match args.get(1) {
            Some(name) => cli_set(name),
            None => {
                eprintln!("Usage: McpHub vault set NOM   (la valeur est lue sur l'entree standard)")
            }
        },
        "get" => match args.get(1) {
            Some(name) => cli_get(name, args.iter().any(|a| a == "--reveal")),
            None => eprintln!("Usage: McpHub vault get NOM [--reveal]"),
        },
        "rm" | "delete" | "remove" => match args.get(1) {
            Some(name) => cli_rm(name),
            None => eprintln!("Usage: McpHub vault rm NOM"),
        },
        "migrate" => cli_migrate(),
        _ => print_help(),
    }
}

fn print_help() {
    eprintln!(
        r#"McpHub vault : secrets dans le Trousseau macOS

  McpHub vault status         Etat du coffre et de secrets.env
  McpHub vault list           Noms des secrets (jamais les valeurs)
  McpHub vault set NOM        Ajoute ou remplace un secret (valeur lue sur stdin, saisie masquee)
                              ex. : pbpaste | McpHub vault set CLOUDFLARE_API_TOKEN
  McpHub vault get NOM        Affiche la valeur masquee (--reveal pour la valeur complete)
  McpHub vault rm NOM         Supprime un secret
  McpHub vault migrate        Copie secrets.env dans le Trousseau, verifie, puis renomme secrets.env

Dans config.json, les secrets s'utilisent comme avant : "${{NOM}}".
Redemarrer le daemon apres une modification : McpHub uninstall && McpHub install"#
    );
}

fn cli_status() {
    let names = list_names();
    println!("Trousseau macOS : disponible (service \"{}\")", service());
    println!("Secrets dans le coffre : {}", names.len());
    match secrets_env_path() {
        Some(p) if p.exists() => {
            let count = fs::read_to_string(&p)
                .map(|c| parse_env_file(&c).len())
                .unwrap_or(0);
            println!(
                "secrets.env : PRESENT ({} variables en clair) -> lancer 'McpHub vault migrate'",
                count
            );
        }
        _ => println!("secrets.env : absent (aucun secret en clair)"),
    }
}

fn cli_list() {
    let names = list_names();
    if names.is_empty() {
        println!("Coffre vide. 'McpHub vault migrate' pour importer secrets.env.");
        return;
    }
    for name in names {
        let state = match get(&name) {
            Ok(Some(_)) => "ok",
            _ => "INTROUVABLE dans le Trousseau",
        };
        println!("  {}  ({})", name, state);
    }
}

fn cli_set(name: &str) {
    if !is_valid_name(name) {
        eprintln!("Nom invalide : lettres, chiffres et _ uniquement (ex. GITHUB_TOKEN).");
        return;
    }
    let value = match read_secret_from_stdin(name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Annule : {}", e);
            return;
        }
    };
    match set(name, &value) {
        Ok(()) => println!("{} enregistre dans le Trousseau ({}).", name, mask(&value)),
        Err(e) => eprintln!("Erreur : {}", e),
    }
}

fn cli_get(name: &str, reveal: bool) {
    match get(name) {
        Ok(Some(v)) if reveal => println!("{}", v),
        Ok(Some(v)) => println!("{} = {}", name, mask(&v)),
        Ok(None) => eprintln!("{} introuvable dans le Trousseau.", name),
        Err(e) => eprintln!("Erreur : {}", e),
    }
}

fn cli_rm(name: &str) {
    match delete(name) {
        Ok(true) => println!("{} supprime.", name),
        Ok(false) => println!(
            "{} n'etait pas dans le Trousseau (retire de l'index).",
            name
        ),
        Err(e) => eprintln!("Erreur : {}", e),
    }
}

fn cli_migrate() {
    let path = match secrets_env_path() {
        Some(p) if p.exists() => p,
        _ => {
            println!("Aucun secrets.env a migrer.");
            return;
        }
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Lecture impossible de {} : {}", path.display(), e);
            return;
        }
    };
    let entries = parse_env_file(&content);
    if entries.is_empty() {
        println!("secrets.env ne contient aucune variable.");
        return;
    }
    let mut failures = Vec::new();
    for (name, value) in &entries {
        if value.is_empty() {
            failures.push(format!("{} (valeur vide)", name));
            continue;
        }
        if let Err(e) = set(name, value) {
            failures.push(format!("{} ({})", name, e));
            continue;
        }
        match get(name) {
            Ok(Some(read_back)) if &read_back == value => {
                println!("  {}  migre", name);
            }
            _ => failures.push(format!("{} (relecture differente)", name)),
        }
    }
    if !failures.is_empty() {
        eprintln!("\nMigration incomplete, secrets.env n'a PAS ete touche :");
        for f in failures {
            eprintln!("  - {}", f);
        }
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_file_name(format!("secrets.env.migre-{}", stamp));
    match fs::rename(&path, &backup) {
        Ok(()) => {
            restrict_permissions(&backup);
            println!(
                "\n{} secrets migres et verifies. secrets.env renomme en {}.",
                entries.len(),
                backup.display()
            );
            println!("Redemarrer McpHub, verifier que tout fonctionne, puis supprimer ce fichier.");
        }
        Err(e) => eprintln!(
            "Secrets migres, mais renommage de secrets.env impossible : {}",
            e
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(is_valid_name("GITHUB_TOKEN"));
        assert!(is_valid_name("_x1"));
        assert!(!is_valid_name("1ABC"));
        assert!(!is_valid_name("A-B"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("A B"));
    }

    #[test]
    fn parses_env_file() {
        let parsed = parse_env_file(
            "# commentaire\nA=1\nexport B=\"deux\"\nC='trois'\n\nbad-name=x\nD = quatre \n",
        );
        assert_eq!(
            parsed,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "deux".into()),
                ("C".into(), "trois".into()),
                ("D".into(), "quatre".into()),
            ]
        );
    }

    #[test]
    fn masks_values() {
        assert_eq!(mask("abc"), "[3 caracteres]");
        assert_eq!(mask("sk_live_1234567890"), "****7890 (18 caracteres)");
    }
}
