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

/// Périmètre d'un secret : le coffre de McpHub (Soflution) ou celui d'un tenant.
/// Chaque tenant a son propre service du Trousseau et son propre index : effacer un tenant
/// ne touche jamais aux secrets Soflution, et l'inverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Hub,
    Tenant(String),
}

impl Scope {
    pub fn service(&self) -> String {
        let base = crate::instance::current().vault_service.clone();
        match self {
            Scope::Hub => base,
            Scope::Tenant(id) => format!("{}-tenant-{}", base, id),
        }
    }

    fn dir(&self) -> PathBuf {
        match self {
            Scope::Hub => crate::instance::hub_dir(),
            Scope::Tenant(id) => crate::tenant::tenant_dir(id),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.dir().join("vault-index.json")
    }

    fn secrets_env_path(&self) -> PathBuf {
        self.dir().join("secrets.env")
    }

    pub fn label(&self) -> String {
        match self {
            Scope::Hub => "McpHub".into(),
            Scope::Tenant(id) => format!("tenant {}", id),
        }
    }
}

fn service() -> String {
    Scope::Hub.service()
}

/// Trousseau explicite (tests, ou trousseau dédié). Par défaut : trousseau de session.
fn keychain_args() -> Vec<String> {
    std::env::var("MCPHUB_KEYCHAIN")
        .ok()
        .filter(|k| !k.is_empty())
        .into_iter()
        .collect()
}

pub fn secrets_env_path() -> Option<PathBuf> {
    Some(Scope::Hub.secrets_env_path())
}

/// `MCPHUB_VAULT_BACKEND=file` force le coffre fichier (tests, machines sans Trousseau).
pub fn is_available() -> bool {
    if std::env::var("MCPHUB_VAULT_BACKEND").is_ok_and(|v| v == "file") {
        return false;
    }
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
    list_names_in(&Scope::Hub)
}

pub fn list_names_in(scope: &Scope) -> Vec<String> {
    if !is_available() {
        return file_backend::read(scope).into_iter().map(|(k, _)| k).collect();
    }
    let path = scope.index_path();
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

fn save_names(scope: &Scope, names: &BTreeSet<String>) -> Result<(), String> {
    let path = scope.index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::json!({
        "service": scope.service(),
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
fn update_names(scope: &Scope, f: impl FnOnce(&mut BTreeSet<String>)) -> Result<(), String> {
    let path = scope.index_path();
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
    let mut names: BTreeSet<String> = list_names_in(scope).into_iter().collect();
    f(&mut names);
    let res = save_names(scope, &names);
    let _ = lock.unlock();
    res
}

fn add_name(scope: &Scope, name: &str) -> Result<(), String> {
    update_names(scope, |names| {
        names.insert(name.to_string());
    })
}

fn remove_name(scope: &Scope, name: &str) -> Result<(), String> {
    update_names(scope, |names| {
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
    get_in(&Scope::Hub, name)
}

pub fn get_in(scope: &Scope, name: &str) -> Result<Option<String>, String> {
    if !is_available() {
        return Ok(file_backend::read(scope).into_iter().find(|(k, _)| k == name).map(|(_, v)| v));
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    // -g écrit la valeur sur stderr sous la forme `password: "texte"` ou, pour les
    // caractères non ASCII, `password: 0xC3A9...  "..."` : on décode l'hexadécimal.
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &scope.service(), "-a", name, "-g"])
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
        if !hex.len().is_multiple_of(2) {
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
    set_in(&Scope::Hub, name, value)
}

pub fn set_in(scope: &Scope, name: &str, value: &str) -> Result<(), String> {
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
        return file_backend::upsert(scope, name, Some(value)).map(|_| ());
    }
    // La commande passe par l'entrée standard de `security -i` : la valeur n'apparaît
    // jamais dans les arguments du processus (invisible pour `ps`).
    // -U : met à jour l'élément s'il existe déjà.
    let mut command = format!(
        "add-generic-password -U -s {} -a {} -l {} -w {}",
        quote_for_security(&scope.service()),
        quote_for_security(name),
        quote_for_security(&format!("McpHub ({}): {}", scope.label(), name)),
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
    match get_in(scope, name) {
        Ok(Some(read_back)) if read_back == value => add_name(scope, name),
        Ok(Some(_)) => Err("relecture differente apres ecriture".into()),
        Ok(None) => Err(format!(
            "echec d'ecriture dans le Trousseau: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(e),
    }
}

pub fn delete_in(scope: &Scope, name: &str) -> Result<bool, String> {
    if !is_available() {
        return file_backend::upsert(scope, name, None);
    }
    if !is_valid_name(name) {
        return Err(format!("nom invalide: {}", name));
    }
    let out = Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", &scope.service(), "-a", name])
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
    remove_name(scope, name)?;
    Ok(removed)
}

/// Efface tout le coffre d'un tenant : chaque nom de l'index, puis tout élément restant du
/// service (créé hors index), puis l'index lui-même. Renvoie le nombre d'éléments effacés.
/// Refuse le coffre McpHub : seul un tenant s'efface en bloc.
pub fn purge(scope: &Scope) -> Result<usize, String> {
    if *scope == Scope::Hub {
        return Err("le coffre McpHub ne s'efface pas en bloc".into());
    }
    let mut removed = 0;
    for name in list_names_in(scope) {
        if delete_in(scope, &name)? {
            removed += 1;
        }
    }
    if is_available() {
        // Éléments du service absents de l'index : on les retire un par un jusqu'au code 44.
        for _ in 0..1000 {
            let out = Command::new("/usr/bin/security")
                .args(["delete-generic-password", "-s", &scope.service()])
                .args(keychain_args())
                .stdin(Stdio::null())
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                if out.status.code() == Some(44) {
                    break;
                }
                return Err(format!(
                    "suppression refusee par le Trousseau: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            removed += 1;
        }
    }
    for path in [scope.index_path(), scope.secrets_env_path()] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{} : {}", path.display(), e)),
        }
    }
    Ok(removed)
}

/// Nombre de lectures du Trousseau lancées en même temps au démarrage.
const VAULT_PARALLEL_READS: usize = 8;

/// Charge dans l'environnement du process les secrets du coffre dont la variable
/// n'est pas déjà définie. Retourne (chargés, introuvables).
pub fn load_into_env() -> (usize, Vec<String>) {
    if !is_available() {
        return (0, Vec::new());
    }
    // Les connexions OAuth (JSON) ne sont pas des variables d'environnement.
    let names_to_read: Vec<String> = list_names()
        .into_iter()
        .filter(|name| !name.starts_with(crate::oauth::VAULT_PREFIX))
        .filter(|name| std::env::var(name).is_err())
        .collect();
    // Chaque lecture lance `/usr/bin/security` (~300 ms, bien plus sur un Mac chargé) :
    // lues une par une, elles retardaient l'ouverture du port de plusieurs secondes.
    let read_values: Vec<(String, Option<String>)> = names_to_read
        .chunks(VAULT_PARALLEL_READS)
        .flat_map(|batch| {
            std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|name| scope.spawn(move || (name.clone(), get(name).ok().flatten())))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("lecture du Trousseau interrompue"))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut loaded = 0;
    let mut missing = Vec::new();
    for (name, value) in read_values {
        match value {
            Some(value) => {
                std::env::set_var(&name, value);
                loaded += 1;
            }
            None => missing.push(name),
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

    use super::Scope;

    pub fn read(scope: &Scope) -> Vec<(String, String)> {
        fs::read_to_string(scope.secrets_env_path())
            .ok()
            .map(|c| super::parse_env_file(&c))
            .unwrap_or_default()
    }

    /// Ajoute, remplace (Some) ou retire (None) une variable. Renvoie vrai si elle existait.
    pub fn upsert(scope: &Scope, name: &str, value: Option<&str>) -> Result<bool, String> {
        let path = scope.secrets_env_path();
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
    // `--tenant <id>` : le coffre de ce tenant au lieu de celui de McpHub.
    let (scope, args) = match crate::tenant::take_tenant_flag(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    if scope != Scope::Hub && matches!(sub, "status" | "migrate") {
        eprintln!("'{}' ne concerne que le coffre McpHub (sans --tenant).", sub);
        std::process::exit(2);
    }
    if !is_available() && matches!(sub, "status" | "migrate") {
        // Hors macOS : le coffre est secrets.env (mode 600, caché aux serveurs isolés).
        let p = crate::instance::hub_dir().join("secrets.env");
        println!("Trousseau macOS indisponible : coffre = {} (mode 600)", p.display());
        println!("Secrets dans le coffre : {}", list_names().len());
        return;
    }
    match sub {
        "status" => cli_status(),
        "list" | "ls" => cli_list(&scope),
        "set" | "add" => match args.get(1) {
            Some(name) => cli_set(&scope, name),
            None => {
                eprintln!("Usage: McpHub vault set [--tenant ID] NOM   (la valeur est lue sur l'entree standard)")
            }
        },
        "get" => match args.get(1) {
            Some(name) => cli_get(&scope, name, args.iter().any(|a| a == "--reveal")),
            None => eprintln!("Usage: McpHub vault get [--tenant ID] NOM [--reveal]"),
        },
        "rm" | "delete" | "remove" => match args.get(1) {
            Some(name) => cli_rm(&scope, name),
            None => eprintln!("Usage: McpHub vault rm [--tenant ID] NOM"),
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

  --tenant ID (avec list, set, get, rm) : coffre isole du tenant ID (service "McpHub-tenant-ID"),
  jamais charge dans l'environnement du demon. ex. : McpHub vault set --tenant acme SUPABASE_ACCESS_TOKEN

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

fn cli_list(scope: &Scope) {
    let names = list_names_in(scope);
    if names.is_empty() {
        match scope {
            Scope::Hub => println!("Coffre vide. 'McpHub vault migrate' pour importer secrets.env."),
            Scope::Tenant(_) => println!("Coffre du {} vide.", scope.label()),
        }
        return;
    }
    for name in names {
        let state = match get_in(scope, &name) {
            Ok(Some(_)) => "ok",
            _ => "INTROUVABLE dans le Trousseau",
        };
        println!("  {}  ({})", name, state);
    }
}

fn cli_set(scope: &Scope, name: &str) {
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
    match set_in(scope, name, &value) {
        Ok(()) => println!("{} enregistre dans le coffre {} ({}).", name, scope.label(), mask(&value)),
        Err(e) => eprintln!("Erreur : {}", e),
    }
}

fn cli_get(scope: &Scope, name: &str, reveal: bool) {
    match get_in(scope, name) {
        Ok(Some(v)) if reveal => println!("{}", v),
        Ok(Some(v)) => println!("{} = {}", name, mask(&v)),
        Ok(None) => eprintln!("{} introuvable dans le Trousseau.", name),
        Err(e) => eprintln!("Erreur : {}", e),
    }
}

fn cli_rm(scope: &Scope, name: &str) {
    match delete_in(scope, name) {
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
