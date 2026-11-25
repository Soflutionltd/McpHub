//! Gestion des serveurs de `config.json` par un agent (`mcphub_call server="mcphub"`) :
//! lister, ajouter, retirer, activer, désactiver. Les confirmations, le refus depuis un dossier
//! tenant (pour tous ces outils) et l'audit sont faits par le proxy ; ce module valide et écrit (écriture protégée :
//! sauvegarde, révision attendue, refus si le fichier change pendant l'opération).

use serde_json::{json, Map, Value};
use std::path::Path;

pub const ADMIN_TOOLS: &[&str] = &[
    "servers_list",
    "server_add",
    "server_remove",
    "server_enable",
    "server_disable",
];

pub fn is_admin_tool(tool: &str) -> bool {
    ADMIN_TOOLS.contains(&tool)
}

/// Outils qui modifient `config.json`.
pub fn is_write_tool(tool: &str) -> bool {
    is_admin_tool(tool) && tool != "servers_list"
}

/// Politique par défaut (une règle explicite de `settings.permissions` l'emporte) : `None` =
/// autorisé, sinon la raison affichée dans la confirmation.
pub fn default_confirmation(tool: &str) -> Option<&'static str> {
    match tool {
        "server_add" => {
            Some("ajoute un serveur MCP : la commande ou l'adresse sera lancee sur cette machine")
        }
        "server_remove" => Some("retire un serveur MCP de la config McpHub (sauvegarde gardee)"),
        "server_enable" => Some("reactive un serveur MCP : sa commande sera relancee"),
        _ => None,
    }
}

/// Ce dont les outils ont besoin, injecté pour les tests.
pub struct Env<'a> {
    pub config_path: &'a Path,
    /// Dossier des bundles installés depuis le registre (`servers/<nom>`).
    pub servers_dir: &'a Path,
    pub tenants: &'a crate::tenant::Tenants,
}

pub fn call(tool: &str, args: &Value, env: &Env) -> Result<Value, String> {
    let revision = args.get("revision").and_then(|v| v.as_str());
    match tool {
        "servers_list" => list(env),
        "server_add" => add(args, revision, env),
        "server_remove" => remove(name_arg(args)?, revision, env),
        "server_enable" => set_disabled(name_arg(args)?, false, revision, env),
        "server_disable" => set_disabled(name_arg(args)?, true, revision, env),
        other => Err(format!(
            "unknown McpHub tool {}: {}",
            other,
            ADMIN_TOOLS.join(", ")
        )),
    }
}

fn name_arg(args: &Value) -> Result<&str, String> {
    args.get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "name required".to_string())
}

fn servers_key(config: &Value) -> &'static str {
    if config.get("servers").and_then(|v| v.as_object()).is_some() {
        "servers"
    } else {
        "mcpServers"
    }
}

fn servers_mut(config: &mut Value) -> &mut Map<String, Value> {
    let key = servers_key(config);
    if !config.get(key).is_some_and(|v| v.is_object()) {
        config[key] = json!({});
    }
    config[key]
        .as_object_mut()
        .expect("objet cree juste au-dessus")
}

/// Tenants qui dépendent d'un serveur (identifiants propres ou serveur partagé).
fn tenants_using(name: &str, tenants: &crate::tenant::Tenants) -> Vec<String> {
    tenants
        .tenants
        .iter()
        .filter(|t| t.owns(name) || t.shared_servers.contains(name))
        .map(|t| t.id.clone())
        .collect()
}

fn origin(entry: &Value) -> &'static str {
    if entry.get("registry").is_some() {
        "registry"
    } else if entry.get("addedBy").is_some() {
        "agent"
    } else {
        "manual"
    }
}

fn list(env: &Env) -> Result<Value, String> {
    let (config, revision) = crate::config::read_raw_with_revision(env.config_path)?;
    let config = config.unwrap_or_else(|| json!({}));
    let cache = crate::cache::load_cache();
    let empty = Map::new();
    let servers = config
        .get(servers_key(&config))
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let mut rows = Vec::new();
    for (name, entry) in servers {
        let mut row = json!({
            "name": name,
            "disabled": entry.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            "origin": origin(entry),
        });
        if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
            row["kind"] = json!("remote");
            row["url"] = json!(crate::redaction::redact_text(url));
            row["auth"] = entry.get("auth").cloned().unwrap_or(Value::Null);
        } else {
            let args: Vec<String> = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            row["kind"] = json!("command");
            row["command"] = json!(crate::redaction::redact_command_preview(
                entry.get("command").and_then(|v| v.as_str()).unwrap_or(""),
                &args
            ));
        }
        if let Some(c) = &cache {
            if let Some(s) = c.servers.get(name) {
                row["tools"] = json!(s.len());
            }
            if let Some(e) = c.errors.get(name) {
                row["error"] = json!(crate::redaction::redact_text(e));
            }
        }
        let used = tenants_using(name, env.tenants);
        if !used.is_empty() {
            row["tenants"] = json!(used);
        }
        rows.push(row);
    }
    Ok(json!({
        "servers": rows,
        "revision": revision,
        "next": "server_add | server_remove | server_enable | server_disable with arguments {name, revision}",
    }))
}

// ─── Validation d'un ajout ───────────────────────────────────

fn valid_name(name: &str) -> Result<(), String> {
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let starts_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if name.is_empty() || name.len() > 64 || !ok_chars || !starts_ok {
        return Err(
            "name: 1 to 64 letters, digits, - or _, starting with a letter or digit".into(),
        );
    }
    if name.eq_ignore_ascii_case("mcphub") {
        return Err("name \"mcphub\" is reserved".into());
    }
    Ok(())
}

/// Noms des `${VAR}` d'une chaîne, et la chaîne sans eux.
fn split_vars(value: &str) -> (Vec<String>, String) {
    let mut vars = Vec::new();
    let mut rest = String::new();
    let mut s = value;
    while let Some(start) = s.find("${") {
        rest.push_str(&s[..start]);
        match s[start..].find('}') {
            Some(end) => {
                let inner = &s[start + 2..start + end];
                let name = inner.split(":-").next().unwrap_or(inner).to_string();
                vars.push(name);
                rest.push(' ');
                s = &s[start + end + 1..];
            }
            None => {
                rest.push_str(&s[start..]);
                s = "";
            }
        }
    }
    rest.push_str(s);
    (vars, rest)
}

const SCHEME_WORDS: &[&str] = &["bearer", "basic", "token"];

/// Une valeur saisie par l'agent : jamais de secret en clair. Pour une clé sensible, seule une
/// référence `${VAR}` (éventuellement précédée de Bearer / Basic / Token) est acceptée.
fn check_value(
    label: &str,
    key: Option<&str>,
    value: &str,
    vars: &mut Vec<String>,
) -> Result<(), String> {
    let (found, rest) = split_vars(value);
    let sensitive = key.is_some_and(crate::redaction::is_sensitive_key);
    let hint = "store the value with `McpHub vault set NAME` (the user types it) and reference it as ${NAME}";
    for token in rest.split_whitespace() {
        let lone = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if sensitive
            && !lone.is_empty()
            && !SCHEME_WORDS.contains(&lone.to_ascii_lowercase().as_str())
        {
            return Err(format!(
                "{}: a secret field accepts only ${{VAR}} references; {}",
                label, hint
            ));
        }
        if crate::redaction::looks_like_secret_value(token) {
            return Err(format!("{}: looks like a secret value; {}", label, hint));
        }
    }
    if sensitive && found.is_empty() && !rest.trim().is_empty() {
        return Err(format!(
            "{}: a secret field accepts only ${{VAR}} references; {}",
            label, hint
        ));
    }
    for v in found {
        if v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{}: invalid variable reference ${{{}}}", label, v));
        }
        vars.push(v);
    }
    Ok(())
}

fn string_map(
    field: &str,
    value: Option<&Value>,
    vars: &mut Vec<String>,
) -> Result<Option<Map<String, Value>>, String> {
    let Some(value) = value else { return Ok(None) };
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{} must be an object of strings", field))?;
    for (k, v) in obj {
        let s = v
            .as_str()
            .ok_or_else(|| format!("{}.{} must be a string", field, k))?;
        check_value(&format!("{}.{}", field, k), Some(k), s, vars)?;
    }
    Ok(if obj.is_empty() {
        None
    } else {
        Some(obj.clone())
    })
}

fn check_args(args: &[String], vars: &mut Vec<String>) -> Result<(), String> {
    for (i, arg) in args.iter().enumerate() {
        let label = format!("args[{}]", i);
        if arg.contains('\n') || arg.contains('\0') {
            return Err(format!("{}: control characters are not allowed", label));
        }
        let previous_flag = i
            .checked_sub(1)
            .map(|p| args[p].as_str())
            .filter(|p| p.starts_with('-') && !p.contains('='));
        if let Some((flag, value)) = arg.split_once('=').filter(|(f, _)| f.starts_with('-')) {
            check_value(&label, Some(flag.trim_start_matches('-')), value, vars)?;
        } else if let Some(flag) = previous_flag.filter(|f| crate::redaction::is_sensitive_flag(f))
        {
            check_value(&label, Some(flag.trim_start_matches('-')), arg, vars)?;
        } else {
            check_value(&label, None, arg, vars)?;
        }
    }
    Ok(())
}

fn check_url(raw: &str, vars: &mut Vec<String>) -> Result<(), String> {
    let (found, _) = split_vars(raw);
    if !found.is_empty() {
        // Une variable dans l'adresse cacherait la destination réelle à la confirmation.
        return Err("url: ${VAR} is not allowed in the address; put secrets in headers".into());
    }
    let url = reqwest::Url::parse(raw).map_err(|e| format!("url: {}", e))?;
    let local = matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]")
    );
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => return Err("url: https only (http only for localhost)".into()),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("url: no credentials in the address; put them in headers as ${VAR}".into());
    }
    for (k, v) in url.query_pairs() {
        check_value(&format!("url query {}", k), Some(k.as_ref()), &v, vars)?;
    }
    Ok(())
}

/// Entrée de config construite et validée à partir des arguments de `server_add`.
fn build_entry(args: &Value, settings: Option<&Value>) -> Result<(Value, Vec<String>), String> {
    let command = args.get("command").and_then(|v| v.as_str());
    let url = args.get("url").and_then(|v| v.as_str());
    let mut vars = Vec::new();
    let mut entry = Map::new();
    match (command, url) {
        (Some(_), Some(_)) | (None, None) => {
            return Err("give either command (local server) or url (remote server)".into())
        }
        (Some(cmd), None) => {
            if cmd.trim().is_empty() || cmd.contains('\n') || cmd.contains('\0') {
                return Err("command must be a non-empty single line".into());
            }
            check_value("command", None, cmd, &mut vars)?;
            let list: Vec<String> = match args.get("args") {
                None => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(String::from)
                            .ok_or_else(|| "args must be an array of strings".to_string())
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err("args must be an array of strings".into()),
            };
            check_args(&list, &mut vars)?;
            entry.insert("command".into(), json!(cmd));
            entry.insert("args".into(), json!(list));
            if let Some(env) = string_map("env", args.get("env"), &mut vars)? {
                entry.insert("env".into(), Value::Object(env));
            }
            // Bac à sable bouclier par défaut, comme une installation depuis le registre (sauf
            // réglage global, qui s'applique alors). L'agent ne peut pas le retirer.
            match args.get("sandbox") {
                Some(Value::Bool(false)) => return Err("sandbox: an agent cannot turn the sandbox off (the user can, in the dashboard)".into()),
                Some(Value::String(s)) if s.eq_ignore_ascii_case("off") => {
                    return Err("sandbox: an agent cannot turn the sandbox off (the user can, in the dashboard)".into())
                }
                Some(v) => {
                    entry.insert("sandbox".into(), v.clone());
                }
                None => {
                    let global = settings.and_then(|s| s.get("sandbox")).filter(|v| !matches!(v, Value::Bool(false)));
                    if global.is_none() {
                        entry.insert("sandbox".into(), json!("shield"));
                    }
                }
            }
        }
        (None, Some(u)) => {
            check_url(u, &mut vars)?;
            entry.insert("url".into(), json!(u));
            if let Some(h) = string_map("headers", args.get("headers"), &mut vars)? {
                entry.insert("headers".into(), Value::Object(h));
            }
            match args.get("auth").and_then(|v| v.as_str()) {
                None => {}
                Some(a) if a == "oauth" || a == "none" => {
                    entry.insert("auth".into(), json!(a));
                }
                Some(_) => return Err("auth must be \"oauth\" or \"none\"".into()),
            }
            if args.get("sandbox").is_some() {
                return Err("sandbox applies to local commands only".into());
            }
        }
    }
    let entry = Value::Object(entry);
    // Même analyse que le démon au chargement (bac à sable compris), variables laissées telles quelles.
    crate::config::parse_server_entry(
        "server_add",
        &entry,
        settings,
        &|s: &str| Ok(s.to_string()),
    )?;
    vars.sort();
    vars.dedup();
    Ok((entry, vars))
}

fn add(args: &Value, revision: Option<&str>, env: &Env) -> Result<Value, String> {
    let name = name_arg(args)?.to_string();
    valid_name(&name)?;
    let done = crate::config::update_raw_guarded(env.config_path, revision, |config| {
        let settings = config.get("settings").cloned();
        let (mut entry, vars) = build_entry(args, settings.as_ref())?;
        let servers = servers_mut(config);
        if let Some(existing) = servers.keys().find(|k| k.eq_ignore_ascii_case(&name)) {
            return Err(format!(
                "a server named {} already exists (server_remove it first, or pick another name)",
                existing
            ));
        }
        entry["addedBy"] = json!({ "by": "agent", "at": crate::audit::iso_utc_now() });
        servers.insert(name.clone(), entry.clone());
        Ok(((entry, vars), true))
    })?;
    let (entry, vars) = done.value;
    let missing: Vec<&String> = vars
        .iter()
        .filter(|v| std::env::var(v.as_str()).is_err())
        .collect();
    let remote = entry.get("url").is_some();
    let oauth = remote
        && match entry.get("auth").and_then(|v| v.as_str()) {
            Some(a) => a == "oauth",
            None => !entry
                .get("headers")
                .and_then(|h| h.as_object())
                .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("authorization"))),
        };
    let mut out = json!({
        "ok": true,
        "name": name,
        "kind": if remote { "remote" } else { "command" },
        "oauth": oauth,
        "config": crate::redaction::redact_json_value(&entry),
        "revision": done.revision,
    });
    if !missing.is_empty() {
        out["missingVariables"] = json!(missing);
        out["warning"] = json!(format!(
            "variable(s) not set: ask the user to run `McpHub vault set NAME` for {}, then the server starts",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(out)
}

fn refuse_if_used(name: &str, action: &str, env: &Env) -> Result<(), String> {
    let used = tenants_using(name, env.tenants);
    if used.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} is used by client account(s) {}: {} it from the McpHub dashboard after checking with the user",
        name,
        used.join(", "),
        action
    ))
}

fn find_name(servers: &Map<String, Value>, name: &str) -> Result<String, String> {
    if servers.contains_key(name) {
        return Ok(name.to_string());
    }
    servers
        .keys()
        .find(|k| k.eq_ignore_ascii_case(name))
        .cloned()
        .ok_or_else(|| format!("no server named {} (servers_list shows them)", name))
}

fn remove(name: &str, revision: Option<&str>, env: &Env) -> Result<Value, String> {
    let done = crate::config::update_raw_guarded(env.config_path, revision, |config| {
        let servers = servers_mut(config);
        let key = find_name(servers, name)?;
        refuse_if_used(&key, "remove", env)?;
        let entry = servers.remove(&key).expect("nom trouve juste au-dessus");
        Ok(((key, entry), true))
    })?;
    let (key, entry) = done.value;
    if entry.get("registry").is_some() {
        let dir = env.servers_dir.join(&key);
        if dir.starts_with(env.servers_dir) && dir != env.servers_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    Ok(json!({
        "ok": true,
        "name": key,
        "removed": crate::redaction::redact_json_value(&entry),
        "backup": done.backup.map(|p| p.display().to_string()),
        "revision": done.revision,
        "note": "secrets stay in the vault; the previous config.json is in the backup",
    }))
}

fn set_disabled(
    name: &str,
    disabled: bool,
    revision: Option<&str>,
    env: &Env,
) -> Result<Value, String> {
    let done = crate::config::update_raw_guarded(env.config_path, revision, |config| {
        let servers = servers_mut(config);
        let key = find_name(servers, name)?;
        let entry = servers
            .get_mut(&key)
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| format!("{}: invalid entry", key))?;
        let current = entry
            .get("disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if current == disabled {
            return Ok(((key, false), false));
        }
        if disabled {
            refuse_if_used(&key, "disable", env)?;
            entry.insert("disabled".into(), json!(true));
        } else {
            entry.remove("disabled");
        }
        Ok(((key, true), true))
    })?;
    let (key, changed) = done.value;
    Ok(json!({
        "ok": true,
        "name": key,
        "disabled": disabled,
        "unchanged": !changed,
        "revision": done.revision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::{TenantDef, Tenants};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    struct Fixture {
        dir: PathBuf,
        tenants: Tenants,
    }

    impl Fixture {
        fn new(tag: &str, config: Value) -> Self {
            let dir =
                std::env::temp_dir().join(format!("mcphub-admin-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("servers")).unwrap();
            std::fs::write(
                dir.join("config.json"),
                serde_json::to_string(&config).unwrap(),
            )
            .unwrap();
            let tenant = TenantDef {
                id: "acme".into(),
                project: "acme".into(),
                workspaces: vec![],
                credentials: BTreeMap::from([("stripe".to_string(), BTreeMap::new())]),
                keep_env: BTreeMap::new(),
                shared_servers: BTreeSet::from(["context7".to_string()]),
            };
            Fixture {
                dir,
                tenants: Tenants {
                    tenants: vec![tenant],
                },
            }
        }

        fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
            let config_path = self.dir.join("config.json");
            let servers_dir = self.dir.join("servers");
            call(
                tool,
                &args,
                &Env {
                    config_path: &config_path,
                    servers_dir: &servers_dir,
                    tenants: &self.tenants,
                },
            )
        }

        fn config(&self) -> Value {
            serde_json::from_str(&std::fs::read_to_string(self.dir.join("config.json")).unwrap())
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn base() -> Value {
        json!({ "mcpServers": {
            "cloudflare": { "command": "npx", "args": ["-y", "cf"] },
            "stripe": { "url": "https://mcp.stripe.com", "auth": "oauth" },
            "context7": { "command": "npx", "args": ["c7"] },
        }})
    }

    #[test]
    fn desactive_puis_reactive_avec_revision() {
        let f = Fixture::new("toggle", base());
        let listed = f.call("servers_list", json!({})).unwrap();
        let rev = listed["revision"].as_str().unwrap().to_string();
        assert_eq!(listed["servers"].as_array().unwrap().len(), 3);

        let off = f
            .call(
                "server_disable",
                json!({"name": "cloudflare", "revision": rev}),
            )
            .unwrap();
        assert_eq!(off["unchanged"], false);
        assert_eq!(f.config()["mcpServers"]["cloudflare"]["disabled"], true);

        let again = f
            .call("server_disable", json!({"name": "cloudflare"}))
            .unwrap();
        assert_eq!(again["unchanged"], true);

        let stale = f.call(
            "server_enable",
            json!({"name": "cloudflare", "revision": rev}),
        );
        assert!(stale.unwrap_err().contains("a change"));

        f.call(
            "server_enable",
            json!({"name": "Cloudflare", "revision": off["revision"]}),
        )
        .unwrap();
        assert!(f.config()["mcpServers"]["cloudflare"]
            .get("disabled")
            .is_none());
    }

    #[test]
    fn serveur_d_un_tenant_ni_retire_ni_desactive() {
        let f = Fixture::new("tenant", base());
        for (tool, name) in [("server_remove", "stripe"), ("server_disable", "context7")] {
            let err = f.call(tool, json!({"name": name})).unwrap_err();
            assert!(err.contains("acme"), "{}", err);
        }
        assert!(f.config()["mcpServers"].get("stripe").is_some());
    }

    #[test]
    fn ajout_commande_avec_bac_a_sable_et_variable() {
        let f = Fixture::new("add", base());
        let out = f
            .call(
                "server_add",
                json!({
                    "name": "weather",
                    "command": "npx",
                    "args": ["-y", "weather-mcp", "--api-key", "${MCPHUB_TEST_ABSENT_VAR}"],
                    "env": { "WEATHER_TOKEN": "${MCPHUB_TEST_ABSENT_VAR}", "REGION": "eu" }
                }),
            )
            .unwrap();
        assert_eq!(out["kind"], "command");
        assert_eq!(out["missingVariables"], json!(["MCPHUB_TEST_ABSENT_VAR"]));
        let entry = &f.config()["mcpServers"]["weather"];
        assert_eq!(entry["sandbox"], "shield");
        assert_eq!(entry["addedBy"]["by"], "agent");
        assert_eq!(entry["env"]["WEATHER_TOKEN"], "${MCPHUB_TEST_ABSENT_VAR}");

        let dup = f
            .call("server_add", json!({"name": "WEATHER", "command": "x"}))
            .unwrap_err();
        assert!(dup.contains("already exists"));
    }

    #[test]
    fn ajout_distant_oauth() {
        let f = Fixture::new("remote", base());
        let out = f
            .call(
                "server_add",
                json!({"name": "linear", "url": "https://mcp.linear.app/mcp", "auth": "oauth"}),
            )
            .unwrap();
        assert_eq!(out["oauth"], true);
        let bearer = f
            .call(
                "server_add",
                json!({"name": "api", "url": "https://api.example.com/mcp", "headers": {"Authorization": "Bearer ${API_TOKEN}"}}),
            )
            .unwrap();
        assert_eq!(bearer["oauth"], false);
    }

    #[test]
    fn secrets_en_clair_refuses() {
        let f = Fixture::new("secrets", base());
        let cases = [
            json!({"name": "a", "command": "npx", "env": {"GITHUB_TOKEN": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"}}),
            json!({"name": "b", "command": "npx", "env": {"API_KEY": "short1"}}),
            json!({"name": "c", "command": "npx", "args": ["--token", "abc"]}),
            json!({"name": "d", "command": "npx", "args": ["--password=hunter2"]}),
            json!({"name": "e", "command": "npx", "args": ["sk-live-0123456789abcdefghijklmnop"]}),
            json!({"name": "f", "url": "https://x.example.com/mcp", "headers": {"Authorization": "Bearer abc123"}}),
            json!({"name": "g", "url": "https://x.example.com/mcp?api_key=abc"}),
            json!({"name": "h", "url": "https://user:pw@x.example.com/mcp"}),
        ];
        for case in cases {
            let err = f.call("server_add", case.clone()).unwrap_err();
            assert!(
                err.contains("secret") || err.contains("credentials") || err.contains("VAR"),
                "{} -> {}",
                case,
                err
            );
        }
        assert_eq!(f.config(), base(), "rien n'est ecrit");
    }

    #[test]
    fn ajouts_invalides_refuses() {
        let f = Fixture::new("invalid", base());
        let cases = [
            json!({"name": "mcphub", "command": "x"}),
            json!({"name": "a b", "command": "x"}),
            json!({"name": "x", "command": "a", "url": "https://a.example.com"}),
            json!({"name": "x"}),
            json!({"name": "x", "url": "http://evil.example.com/mcp"}),
            json!({"name": "x", "url": "https://${HOST}/mcp"}),
            json!({"name": "x", "command": "npx", "sandbox": false}),
            json!({"name": "x", "command": "npx", "sandbox": "off"}),
            json!({"name": "x", "command": "npx", "args": "-y pkg"}),
            json!({"name": "x", "url": "https://a.example.com", "auth": "basic"}),
        ];
        for case in cases {
            assert!(f.call("server_add", case.clone()).is_err(), "{}", case);
        }
        assert_eq!(f.config(), base());
    }

    #[test]
    fn retrait_avec_sauvegarde_et_bundle() {
        let mut config = base();
        config["mcpServers"]["bundle"] = json!({"command": "node", "registry": {"name": "io.x/b"}});
        let f = Fixture::new("remove", config);
        std::fs::create_dir_all(f.dir.join("servers/bundle")).unwrap();
        let out = f.call("server_remove", json!({"name": "bundle"})).unwrap();
        assert!(out["backup"].as_str().unwrap().contains("backups"));
        assert!(!f.dir.join("servers/bundle").exists());
        assert!(f.config()["mcpServers"].get("bundle").is_none());
        assert!(f.call("server_remove", json!({"name": "bundle"})).is_err());
    }

    #[test]
    fn variables_reperees() {
        assert_eq!(split_vars("Bearer ${A}").0, vec!["A"]);
        assert_eq!(split_vars("${A:-x}/${B}").0, vec!["A", "B"]);
        assert!(check_value("k", Some("TOKEN"), "${A}", &mut vec![]).is_ok());
        assert!(check_value("k", Some("TOKEN"), "${A} extra", &mut vec![]).is_err());
        assert!(check_value("k", None, "@scope/pkg@latest", &mut vec![]).is_ok());
    }
}
