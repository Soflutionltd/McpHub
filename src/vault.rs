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

fn service() -> String {
    crate::instance::current().vault_service.clone()
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
    Some(crate::instance::hub_dir())
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
    if !is_available() {
        return file_backend::read().into_iter().map(|(k, _)| k).collect();
    }
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
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    restrict_permissions(&tmp);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Lecture-modification-écriture de l'index sous verrou inter-processus.
fn update_names(f: impl FnOnce(&mut BTreeSet<String>)) -> Result<(), String> {
    let path = index_path().ok_or("dossier ~/.McpHub introuvable")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut names: BTreeSet<String> = list_names().into_iter().collect();
    f(&mut names);
    let res = save_names(&names);
    let _ = lock.unlock();
    res
}

fn add_name(name: &str) -> Result<(), String> {
    update_names(|names| {
        names.insert(name.to_string());
    })
}

fn remove_name(name: &str) -> Result<(), String> {
    update_names(|names| {
        names.remove(name);
    })
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
        return Ok(file_backend::read().into_iter().find(|(k, _)| k == name).map(|(_, v)| v));
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    // -g écrit la valeur sur stderr sous la forme `password: "texte"` ou, pour les
    // caractères non ASCII, `password: 0xC3A9...  "..."` : on décode l'hexadécimal.
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service(), "-a", name, "-g"])
        .args(keychain_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        // Code 44 = élément introuvable ; tout autre code est une vraie erreur
        // (trousseau verrouillé, accès refusé...).
        if out.status.code() == Some(44) {
            return Ok(None);
        }
        return Err(format!(
            "Trousseau: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr
        .lines()
        .find_map(|l| l.strip_prefix("password: "))
        .and_then(parse_password_line)
    {
        Some(value) => Ok(Some(value)),
        None => Err("Trousseau: format de reponse inattendu".into()),
    }
}

/// Décode la ligne `password:` de `security -g`.
fn parse_password_line(rest: &str) -> Option<String> {
    let rest = rest.trim_end();
    if let Some(hex) = rest.strip_prefix("0x") {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() % 2 != 0 {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        return bytes.map(|b| String::from_utf8_lossy(&b).to_string());
    }
    if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
        return Some(rest[1..rest.len() - 1].to_string());
    }
    if rest.is_empty() {
        return Some(String::new());
    }
    None
}

/// Met une valeur entre guillemets pour le mode interactif de `security`
/// (le caractère `\` échappe le suivant).
fn quote_for_security(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn set(name: &str, value: &str) -> Result<(), String> {
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    if value.is_empty() {
        return Err("valeur vide".into());
    }
    if value.contains('\n') || value.contains('\r') {
        return Err("valeur sur plusieurs lignes non prise en charge".into());
    }
    if !is_available() {
        return file_backend::upsert(name, Some(value)).map(|_| ());
    }
    // La commande passe par l'entrée standard de `security -i` : la valeur n'apparaît
    // jamais dans les arguments du processus (invisible pour `ps`).
    // -U : met à jour l'élément s'il existe déjà.
    let mut command = format!(
        "add-generic-password -U -s {} -a {} -l {} -w {}",
        quote_for_security(&service()),
        quote_for_security(name),
        quote_for_security(&format!("McpHub: {}", name)),
        quote_for_security(value)
    );
    for k in keychain_args() {
        command.push(' ');
        command.push_str(&quote_for_security(&k));
    }
    command.push('\n');
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(command.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    // `security -i` renvoie 0 même si la commande échoue : on vérifie en relisant.
    match get(name) {
        Ok(Some(read_back)) if read_back == value => add_name(name),
        Ok(Some(_)) => Err("relecture differente apres ecriture".into()),
        Ok(None) => Err(format!(
            "echec d'ecriture dans le Trousseau: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(e),
    }
}

pub fn delete(name: &str) -> Result<bool, String> {
    if !is_available() {
        return file_backend::upsert(name, None);
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
    let removed = out.status.success();
    if !removed && out.status.code() != Some(44) {
        // Erreur réelle : on garde le nom dans l'index pour ne pas perdre la trace de l'élément.
        return Err(format!(
            "suppression refusee par le Trousseau: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    remove_name(name)?;
    Ok(removed)
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
        // Les connexions OAuth (JSON) ne sont pas des variables d'environnement.
        if name.starts_with(crate::oauth::VAULT_PREFIX) {
            continue;
        }
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

/// Sans Trousseau (Linux, serveurs) : les secrets vivent dans ~/.McpHub/secrets.env, en mode 600,
/// jamais lisible par les serveurs isolés (dossier caché par le bac à sable).
mod file_backend {
    use std::fs;
    use std::io::Write;

    pub fn read() -> Vec<(String, String)> {
        super::secrets_env_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|c| super::parse_env_file(&c))
            .unwrap_or_default()
    }

    /// Ajoute, remplace (Some) ou retire (None) une variable. Renvoie vrai si elle existait.
    pub fn upsert(name: &str, value: Option<&str>) -> Result<bool, String> {
        let path = super::secrets_env_path().ok_or("dossier ~/.McpHub introuvable")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("env.lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut existed = false;
        let mut lines: Vec<String> = Vec::new();
        for line in content.lines() {
            let t = line.trim();
            let key = t.strip_prefix("export ").unwrap_or(t).split('=').next().unwrap_or("").trim();
            if !t.starts_with('#') && key == name {
                existed = true;
                continue;
            }
            lines.push(line.to_string());
        }
        if let Some(v) = value {
            lines.push(format!("{}={}", name, v));
        }
        let tmp = path.with_extension(format!("env.tmp.{}", std::process::id()));
        let res = (|| -> std::io::Result<()> {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all((lines.join("\n") + "\n").as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        })();
        let _ = lock.unlock();
        res.map_err(|e| e.to_string())?;
        Ok(existed)
    }
}

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

pub(crate) fn read_secret_from_stdin(name: &str) -> Result<String, String> {
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
        eprintln!(
            "McpHub continue d'utiliser {}.",
            crate::instance::hub_dir().join("secrets.env").display()
        );
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
    fn parses_security_password_lines() {
        assert_eq!(parse_password_line("\"abc123\""), Some("abc123".into()));
        assert_eq!(
            parse_password_line("0xC3A974C3A9E286926F6B  \"\\303\\251t\""),
            Some("été→ok".into())
        );
        assert_eq!(parse_password_line("0xABC"), None);
        assert_eq!(parse_password_line("garbage"), None);
    }

    #[test]
    fn quotes_for_security_interactive_mode() {
        assert_eq!(quote_for_security("a b"), "\"a b\"");
        assert_eq!(quote_for_security("x\"y\\z"), "\"x\\\"y\\\\z\"");
    }

    #[test]
    fn masks_values() {
        assert_eq!(mask("abc"), "[3 caracteres]");
        assert_eq!(mask("sk_live_1234567890"), "****7890 (18 caracteres)");
    }
}
