//! Cloisonnement des tenants, de bout en bout : le vrai binaire (`McpHub stdio`), un faux
//! serveur MCP qui renvoie ce qu'il reçoit dans son environnement, un coffre fichier et un
//! projects.json temporaires. Jamais la config, les projets ni le Trousseau de l'utilisateur.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// Faux serveur : l'outil `get_identity` (lecture, donc mis en cache) renvoie le jeton, le
/// HOME, la clé .p8 littérale et une variable héritée du démon qu'il a reçus.
const FAKE_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"get_identity","description":"Identity","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"token=%s home=%s p8=%s leak=%s ssh=%s"}]}}\n' "$id" "$TOKEN" "$HOME" "$P8" "$LEAK" "$SSH_AUTH_SOCK" ;;
    *'"method":"ping"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;

struct Fixture {
    root: PathBuf,
    hub: PathBuf,
    acme: PathBuf,
    homes: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("tenant-iso-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let hub = root.join("hub");
        let acme = root.join("clients").join("acme");
        let homes = root.join("homes");
        for dir in [&hub, &acme, &root.join("sofapp"), &hub.join("tenants").join("acme")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let script = root.join("fake-server.sh");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let server = |token: &str| {
            json!({
                "command": "/bin/sh",
                "args": [script.to_string_lossy()],
                "env": {
                    "TOKEN": token,
                    "PATH": "/usr/bin:/bin",
                    "P8": "/hub/private_keys/AuthKey.p8"
                },
                "inheritEnv": ["LEAK"],
                "sandbox": false
            })
        };
        let config = json!({
            "servers": { "svc": server("${HUB_TOKEN}"), "other": server("${HUB_TOKEN}") },
            "settings": { "preload": "none" }
        });
        std::fs::write(hub.join("config.json"), serde_json::to_string_pretty(&config).unwrap()).unwrap();
        // Coffre McpHub (fichier, hors Trousseau) et coffre du tenant.
        std::fs::write(hub.join("secrets.env"), "HUB_TOKEN=hub-secret\n").unwrap();
        std::fs::write(hub.join("tenants").join("acme").join("secrets.env"), "ACME_TOKEN=acme-secret\n").unwrap();
        let projects = json!({
            "projects": {
                "sofapp": { "workspaces": [root.join("sofapp")], "allowedServers": ["*"] },
                "acme": {
                    "workspaces": [acme],
                    "tenant": "acme",
                    "credentials": { "svc": { "TOKEN": "vault:acme/ACME_TOKEN" } },
                    "sharedServers": []
                }
            }
        });
        std::fs::write(hub.join("projects.json"), serde_json::to_string_pretty(&projects).unwrap()).unwrap();
        Self { root, hub, acme, homes }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_McpHub"));
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("user-home"))
            .env("MCPHUB_HOME", &self.hub)
            .env("MCPHUB_VAULT_BACKEND", "file")
            .env("MCPHUB_PROJECTS", self.hub.join("projects.json"))
            .env("MCPHUB_TENANT_HOMES", &self.homes)
            .env("LEAK", "daemon-only")
            .env("SSH_AUTH_SOCK", "/tmp/agent.sock");
        cmd
    }

    fn start(&self) -> Hub {
        let mut child = self
            .command()
            .arg("stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if std::env::var("MCPHUB_TEST_STDERR").is_ok() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .expect("McpHub stdio");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut hub = Hub { child, stdin: Some(stdin), stdout, next_id: 1 };
        hub.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} }));
        hub
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Hub {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Hub {
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let stdin = self.stdin.as_mut().expect("entree standard ouverte");
        writeln!(stdin, "{}", line).unwrap();
        stdin.flush().unwrap();
        loop {
            let mut buf = String::new();
            assert!(self.stdout.read_line(&mut buf).unwrap() > 0, "McpHub s'est arrete");
            let Ok(msg) = serde_json::from_str::<Value>(&buf) else { continue };
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    /// `mcphub_call` avec le dossier de l'appelant ; renvoie le texte ou l'erreur.
    fn call(&mut self, server: &str, working_dir: Option<&Path>, cwd: Option<&Path>) -> Result<String, String> {
        let mut ctx = serde_json::Map::new();
        if let Some(w) = working_dir {
            ctx.insert("working_dir".into(), json!(w));
        }
        if let Some(c) = cwd {
            ctx.insert("cwd".into(), json!(c));
        }
        let mut args = json!({ "server": server, "tool": "get_identity", "arguments": {} });
        if !ctx.is_empty() {
            args["_mcphub_context"] = Value::Object(ctx);
        }
        let resp = self.request("tools/call", json!({ "name": "mcphub_call", "arguments": args }));
        if let Some(err) = resp.get("error") {
            return Err(err["message"].as_str().unwrap_or_default().to_string());
        }
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
        if resp["result"]["isError"] == json!(true) {
            return Err(text);
        }
        Ok(text)
    }
}

impl Hub {
    /// Arrêt propre : fin de l'entrée standard, puis attente de la sortie du processus.
    fn finish(mut self) {
        drop(self.stdin.take());
        for _ in 0..100 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn un_tenant_n_utilise_jamais_les_jetons_mcphub_et_inversement() {
    let fx = Fixture::new("isolation");
    let mut hub = fx.start();
    let sofapp = fx.root.join("sofapp");
    let acme_src = fx.acme.join("site").join("src");

    // Sans dossier connu, puis depuis un projet McpHub : comptes McpHub.
    for wd in [None, Some(sofapp.as_path())] {
        let out = hub.call("svc", wd, None).unwrap();
        assert!(out.contains("token=hub-secret"), "{}", out);
        assert!(!out.contains("acme-secret"), "{}", out);
    }

    // Depuis le dossier du tenant : son instance, son jeton, rien du démon.
    let out = hub.call("svc", Some(&acme_src), None).unwrap();
    assert!(out.contains("token=acme-secret"), "{}", out);
    assert!(!out.contains("hub-secret"), "{}", out);
    assert!(out.contains(&format!("home={}", fx.homes.join("acme").join("home").display())), "{}", out);
    assert!(out.contains("p8= "), "cle .p8 McpHub non transmise : {}", out);
    assert!(out.contains("leak= "), "variable du demon non transmise : {}", out);
    assert!(out.ends_with("ssh="), "agent SSH non transmis : {}", out);

    // Lecture mise en cache : chaque périmètre garde la sienne.
    let again = hub.call("svc", None, None).unwrap();
    assert!(again.contains("token=hub-secret"), "cache partage avec le tenant : {}", again);
    let again = hub.call("svc", None, Some(&fx.acme)).unwrap();
    assert!(again.contains("token=acme-secret"), "dossier courant du client ignore : {}", again);

    // Depuis le tenant, un serveur qui n'est pas le sien est refusé (pas de repli McpHub).
    let err = hub.call("other", Some(&fx.acme), None).unwrap_err();
    assert!(err.contains("tenant acme"), "{}", err);

    // Une instance de tenant ne s'appelle jamais par son nom, d'où qu'on vienne.
    for wd in [None, Some(sofapp.as_path()), Some(fx.acme.as_path())] {
        let err = hub.call("svc@acme", wd, None).unwrap_err();
        assert!(err.contains("instance de tenant"), "{}", err);
    }

    // Un client lancé dans le dossier du tenant ne vise pas un autre dossier.
    let err = hub.call("svc", Some(&sofapp), Some(&fx.acme)).unwrap_err();
    assert!(err.contains("tenant acme"), "{}", err);
    hub.finish();

    // Journal d'audit : le tenant de chaque appel, jamais la valeur d'un secret. Écrit en
    // tâche de fond : on lui laisse quelques instants.
    let expected = |entries: &[Value]| {
        entries.iter().any(|e| e["tenant"] == "acme" && e["status"] == "ok" && e["server"] == "svc")
            && entries.iter().any(|e| e["tenant"] == "acme" && e["decision"] == "deny" && e["server"] == "other")
            && entries.iter().any(|e| e.get("tenant").is_none() && e["status"] == "ok")
    };
    let mut audit = String::new();
    for _ in 0..50 {
        audit = std::fs::read_to_string(fx.hub.join("audit.jsonl")).unwrap_or_default();
        let entries: Vec<Value> = audit.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        if expected(&entries) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let entries: Vec<Value> = audit.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    assert!(expected(&entries), "{}", audit);
    assert!(!audit.contains("acme-secret") && !audit.contains("hub-secret"));
}

#[test]
fn une_confirmation_ne_vaut_que_pour_son_perimetre() {
    let fx = Fixture::new("confirmation");
    let path = fx.hub.join("config.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["settings"]["permissions"] = json!({
        "confirmVia": "token",
        "rules": [{ "server": "svc", "tool": "get_identity", "action": "confirm" }]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    let mut hub = fx.start();
    let call = |hub: &mut Hub, wd: Option<&Path>, token: Option<&str>| {
        let mut args = json!({ "server": "svc", "tool": "get_identity", "arguments": {} });
        if let Some(w) = wd {
            args["_mcphub_context"] = json!({ "working_dir": w });
        }
        if let Some(t) = token {
            args["confirmation_token"] = json!(t);
        }
        hub.request("tools/call", json!({ "name": "mcphub_call", "arguments": args }))
    };
    // Jeton délivré pour le compte McpHub.
    let asked = call(&mut hub, None, None);
    let token = asked["result"]["_meta"]["mcphub"]["confirmation_token"].as_str().unwrap().to_string();
    // Présenté depuis le dossier du tenant : refusé, l'appel ne part pas.
    let reused = call(&mut hub, Some(&fx.acme), Some(&token));
    let text = reused.to_string();
    assert!(!text.contains("token=acme-secret"), "{}", text);
    assert!(text.contains("Jeton de confirmation invalide"), "{}", text);
    // La demande faite depuis le tenant nomme son instance : l'utilisateur sait quel compte.
    let tenant_ask = call(&mut hub, Some(&fx.acme), None).to_string();
    assert!(tenant_ask.contains("svc@acme"), "{}", tenant_ask);
}

#[test]
fn gestion_des_serveurs_par_l_agent() {
    let fx = Fixture::new("admin");
    let path = fx.hub.join("config.json");
    let mut config = read_json(&path);
    config["settings"]["permissions"] = json!({ "confirmVia": "token" });
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    let projects_path = fx.hub.join("projects.json");
    let mut projects = read_json(&projects_path);
    projects["projects"]["acme"]["sharedServers"] = json!(["mcphub"]);
    std::fs::write(&projects_path, serde_json::to_string_pretty(&projects).unwrap()).unwrap();
    let mut hub = fx.start();
    let admin = |hub: &mut Hub, tool: &str, arguments: Value, wd: Option<&Path>, token: Option<&str>| {
        let mut args = json!({ "server": "mcphub", "tool": tool, "arguments": arguments });
        if let Some(w) = wd {
            args["_mcphub_context"] = json!({ "working_dir": w });
        }
        if let Some(t) = token {
            args["confirmation_token"] = json!(t);
        }
        hub.request("tools/call", json!({ "name": "mcphub_call", "arguments": args }))
    };
    let text = |resp: &Value| -> Value {
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap_or("null")).unwrap_or(Value::Null)
    };

    // Liste : le serveur utilisé par le tenant est signalé, la valeur du jeton n'apparaît pas.
    let listed = admin(&mut hub, "servers_list", json!({}), None, None);
    let listed_text = listed.to_string();
    assert!(!listed_text.contains("hub-secret"), "{}", listed_text);
    let rows = text(&listed);
    let svc = rows["servers"].as_array().unwrap().iter().find(|r| r["name"] == "svc").unwrap().clone();
    assert_eq!(svc["tenants"], json!(["acme"]));
    let revision = rows["revision"].as_str().unwrap().to_string();

    // Désactiver : sans confirmation, avec sauvegarde.
    let off = admin(&mut hub, "server_disable", json!({ "name": "other", "revision": revision }), None, None);
    assert_eq!(text(&off)["ok"], true, "{}", off);
    assert_eq!(read_json(&path)["servers"]["other"]["disabled"], true);
    assert!(std::fs::read_dir(fx.hub.join("backups")).unwrap().count() >= 1);

    // Même avec "mcphub" partagé, aucune modification depuis le dossier du tenant.
    let from_tenant = admin(&mut hub, "server_enable", json!({ "name": "other" }), Some(&fx.acme), None).to_string();
    assert!(from_tenant.contains("toujours refuse"), "{}", from_tenant);
    assert_eq!(read_json(&path)["servers"]["other"]["disabled"], true);
    let list_from_tenant = admin(&mut hub, "servers_list", json!({}), Some(&fx.acme), None).to_string();
    assert!(list_from_tenant.contains("toujours refuse"), "{}", list_from_tenant);

    // Ajouter : confirmation obligatoire, puis ajout avec le jeton.
    let add_args = json!({ "name": "extra", "command": "/bin/echo", "env": { "EXTRA_TOKEN": "${EXTRA_ABSENT_TOKEN}" } });
    let asked = admin(&mut hub, "server_add", add_args.clone(), None, None);
    assert_eq!(asked["result"]["_meta"]["mcphub"]["status"], "confirmation_required", "{}", asked);
    assert!(read_json(&path)["servers"].get("extra").is_none());
    let token = asked["result"]["_meta"]["mcphub"]["confirmation_token"].as_str().unwrap().to_string();
    let added = text(&admin(&mut hub, "server_add", add_args, None, Some(&token)));
    assert_eq!(added["ok"], true, "{}", added);
    assert_eq!(added["missingVariables"], json!(["EXTRA_ABSENT_TOKEN"]));
    assert_eq!(read_json(&path)["servers"]["extra"]["sandbox"], "shield");

    // Secret en clair : refusé, même avec confirmation.
    let leak = json!({ "name": "leak", "command": "/bin/echo", "env": { "API_KEY": "plain-value-123" } });
    let asked = admin(&mut hub, "server_add", leak.clone(), None, None);
    let token = asked["result"]["_meta"]["mcphub"]["confirmation_token"].as_str().unwrap().to_string();
    let refused = admin(&mut hub, "server_add", leak, None, Some(&token));
    assert_eq!(refused["result"]["isError"], true, "{}", refused);
    assert!(read_json(&path)["servers"].get("leak").is_none());
    hub.finish();
}

#[test]
fn secret_absent_bloque_l_instance_sans_repli() {
    let fx = Fixture::new("manquant");
    std::fs::write(fx.hub.join("tenants").join("acme").join("secrets.env"), "").unwrap();
    let mut hub = fx.start();
    let err = hub.call("svc", Some(&fx.acme), None).unwrap_err();
    assert!(err.contains("ACME_TOKEN"), "{}", err);
    assert!(!err.contains("hub-secret"), "{}", err);
}

#[test]
fn projects_json_invalide_refuse_les_appels_situes() {
    let fx = Fixture::new("invalide");
    std::fs::write(fx.hub.join("projects.json"), "{ pas du json").unwrap();
    let mut hub = fx.start();
    let err = hub.call("svc", Some(&fx.acme), None).unwrap_err();
    assert!(err.contains("projects.json invalide"), "{}", err);
}

#[test]
fn cli_tenant_et_coffre_du_tenant() {
    let fx = Fixture::new("cli");
    let beta = fx.root.join("clients").join("beta");
    std::fs::create_dir_all(&beta).unwrap();
    let run = |args: &[&str], stdin: Option<&str>| {
        let mut child = fx
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = stdin {
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        )
    };

    let (ok, out) = run(&["tenant", "add", "beta", "--dir", beta.to_str().unwrap()], None);
    assert!(ok, "{}", out);
    let backups = std::fs::read_dir(&fx.hub)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("projects.json.avant-tenant-beta-"))
        .count();
    assert_eq!(backups, 1, "sauvegarde de projects.json");

    // Secret du tenant : dans son coffre, pas dans celui de McpHub.
    let (ok, out) = run(&["vault", "set", "--tenant", "beta", "BETA_TOKEN"], Some("beta-secret\n"));
    assert!(ok, "{}", out);
    assert!(!out.contains("beta-secret"), "valeur affichee : {}", out);
    let tenant_env = std::fs::read_to_string(fx.hub.join("tenants").join("beta").join("secrets.env")).unwrap();
    assert!(tenant_env.contains("BETA_TOKEN=beta-secret"));
    let hub_env = std::fs::read_to_string(fx.hub.join("secrets.env")).unwrap();
    assert!(!hub_env.contains("BETA_TOKEN"));

    // Tenant inconnu : refusé (pas de secret rangé au mauvais endroit).
    let (ok, _) = run(&["vault", "set", "--tenant", "gamma", "X"], Some("x\n"));
    assert!(!ok);

    let (ok, out) = run(&["tenant", "list"], None);
    assert!(ok, "{}", out);
    assert!(out.contains("beta") && out.contains("acme"), "{}", out);

    // rm efface tout le coffre du tenant et le retire de projects.json.
    let (ok, out) = run(&["tenant", "rm", "beta", "--yes"], None);
    assert!(ok, "{}", out);
    assert!(!fx.hub.join("tenants").join("beta").exists());
    let projects: Value =
        serde_json::from_str(&std::fs::read_to_string(fx.hub.join("projects.json")).unwrap()).unwrap();
    assert!(projects["projects"]["beta"].get("tenant").is_none());
    assert_eq!(projects["projects"]["acme"]["tenant"], "acme", "les autres tenants restent");
    // Le coffre McpHub n'a pas bougé.
    assert_eq!(std::fs::read_to_string(fx.hub.join("secrets.env")).unwrap(), "HUB_TOKEN=hub-secret\n");
}

// ---------------------------------------------------------------------------
// Écran Client accounts : routes HTTP du tableau de bord (`McpHub serve`)
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn start(fx: &Fixture) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = fx
            .command()
            .arg("serve")
            .env("MCPHUB_INSTANCE", "tenanthttp")
            .env("MCPHUB_PORT", port.to_string())
            .env("MCPHUB_AUTO_UPDATE", "0")
            .env("MCPHUB_NO_DIALOG", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if std::env::var("MCPHUB_TEST_STDERR").is_ok() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .expect("McpHub serve");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(std::time::Instant::now() < deadline, "le demon n'ecoute pas");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Le jeton est créé par le démon à la première requête.
        let mut daemon = Self { child, port, token: String::new() };
        let (status, _) = daemon.raw("GET", "/api/tenants", None, None, None);
        assert_eq!(status, 401);
        daemon.token = std::fs::read_to_string(fx.hub.join("auth-token")).expect("jeton du demon").trim().to_string();
        daemon
    }

    /// Requête comme le tableau de bord : Host et Origin locaux, jeton Bearer.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
        let origin = format!("http://127.0.0.1:{}", self.port);
        self.raw(method, path, body.map(|b| b.to_string()), Some(&self.token), Some(&origin))
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let (status, text) = self.call(method, path, body);
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn raw(&self, method: &str, path: &str, body: Option<String>, token: Option<&str>, origin: Option<&str>) -> (u16, String) {
        use std::io::Read;
        let body = body.unwrap_or_default();
        let mut request = format!(
            "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            method,
            path,
            self.port,
            body.len()
        );
        if let Some(t) = token {
            request.push_str(&format!("Authorization: Bearer {}\r\n", t));
        }
        if let Some(o) = origin {
            request.push_str(&format!("Origin: {}\r\n", o));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let text = response.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        (status, text)
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn ecran_client_accounts_par_http() {
    let fx = Fixture::new("http");
    // Projets cf-orchestrator (lecture seule) : un projet du client, un projet McpHub.
    let cf_dir = fx.root.join("user-home").join(".cf-orchestrator");
    std::fs::create_dir_all(&cf_dir).unwrap();
    std::fs::write(
        cf_dir.join("projects.json"),
        json!({ "projects": {
            "acme-site": { "type": "pages", "cwd": "/x", "build_cmd": "b", "cf_project_name": "acme-pages", "tenant": "acme" },
            "hub-site": { "type": "pages", "cwd": "/y", "build_cmd": "b", "cf_project_name": "hub-pages" }
        }})
        .to_string(),
    )
    .unwrap();
    let daemon = Daemon::start(&fx);
    let projects_path = fx.hub.join("projects.json");
    let tenant_secrets = fx.hub.join("tenants").join("acme").join("secrets.env");

    // Sans jeton, ou depuis une autre origine : refusé avant toute action.
    let (status, _) = daemon.raw("GET", "/api/tenants", None, None, None);
    assert_eq!(status, 401);
    let evil = json!({ "value": "pirate" }).to_string();
    let (status, _) = daemon.raw(
        "PUT",
        "/api/tenants/acme/secrets/ACME_TOKEN",
        Some(evil),
        Some(&daemon.token),
        Some("http://evil.example"),
    );
    assert_eq!(status, 403);
    assert!(std::fs::read_to_string(&tenant_secrets).unwrap().contains("ACME_TOKEN=acme-secret"));

    // Lecture : noms et états seulement, jamais une valeur.
    let (status, text) = daemon.call("GET", "/api/tenants", None);
    assert_eq!(status, 200, "{}", text);
    assert!(!text.contains("acme-secret") && !text.contains("hub-secret"), "valeur de secret renvoyee : {}", text);
    let snapshot: Value = serde_json::from_str(&text).unwrap();
    let acme = &snapshot["tenants"][0];
    assert_eq!(acme["id"], "acme");
    assert_eq!(acme["servers"][0]["secrets"][0], json!({ "var": "TOKEN", "name": "ACME_TOKEN", "present": true, "connector": null }));
    assert_eq!(acme["cf"]["projects"], json!([{ "name": "acme-site", "cf_project_name": "acme-pages", "type": "pages" }]));
    let svc = snapshot["available"].as_array().unwrap().iter().find(|a| a["name"] == "svc").unwrap();
    assert_eq!(svc["required"], json!(["TOKEN"]));
    assert_eq!(svc["optional"], json!(["P8"]));
    let version = snapshot["version"].as_str().unwrap().to_string();

    // Écriture d'un secret : la valeur part au coffre du client et ne revient jamais.
    let (status, text) = daemon.call(
        "PUT",
        "/api/tenants/acme/secrets/ACME_TOKEN",
        Some(&json!({ "value": "acme-rotated" })),
    );
    assert_eq!(status, 200, "{}", text);
    assert!(!text.contains("acme-rotated"));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["replaced"], true);
    assert!(std::fs::read_to_string(&tenant_secrets).unwrap().contains("ACME_TOKEN=acme-rotated"));
    // Le coffre McpHub n'est pas touché.
    assert!(std::fs::read_to_string(fx.hub.join("secrets.env")).unwrap().contains("HUB_TOKEN=hub-secret"));

    // Seuls les secrets déclarés par les serveurs du client s'écrivent ici.
    for name in ["HUB_TOKEN", "MCPHUB_OAUTH_SVC", "bad-name"] {
        let (status, _) =
            daemon.call("PUT", &format!("/api/tenants/acme/secrets/{}", name), Some(&json!({ "value": "x" })));
        assert_eq!(status, 400, "{}", name);
    }
    let (status, _) =
        daemon.call("PUT", "/api/tenants/acme/secrets/ACME_TOKEN", Some(&json!({ "value": "a\nb" })));
    assert_eq!(status, 400);
    let big = json!({ "value": "x".repeat(17 * 1024) });
    let (status, _) = daemon.call("PUT", "/api/tenants/acme/secrets/ACME_TOKEN", Some(&big));
    assert_eq!(status, 413);

    // Serveurs du client : version périmée refusée, puis écriture avec sauvegarde.
    let servers = json!({ "servers": { "svc": ["TOKEN", "P8"] }, "shared": ["other"], "version": "0-0" });
    let (status, _) = daemon.json("PUT", "/api/tenants/acme/servers", Some(&servers));
    assert_eq!(status, 409);
    let unknown = json!({ "servers": { "nope": [] }, "shared": [], "version": version });
    let (status, _) = daemon.json("PUT", "/api/tenants/acme/servers", Some(&unknown));
    assert_eq!(status, 400);
    let servers = json!({ "servers": { "svc": ["TOKEN", "P8"] }, "shared": ["other"], "version": version });
    let (status, body) = daemon.json("PUT", "/api/tenants/acme/servers", Some(&servers));
    assert_eq!(status, 200, "{}", body);
    assert!(Path::new(body["backup"].as_str().unwrap()).exists());
    let saved = read_json(&projects_path);
    assert_eq!(
        saved["projects"]["acme"]["credentials"]["svc"],
        json!({ "TOKEN": "vault:acme/ACME_TOKEN", "P8": "vault:acme/P8" })
    );
    assert_eq!(saved["projects"]["acme"]["sharedServers"], json!(["other"]));

    // Vérification : le secret P8 manque, sans jamais citer de valeur.
    let (status, body) = daemon.json("POST", "/api/tenants/acme/check", None);
    assert_eq!(status, 200);
    let result = &body["results"][0];
    assert_eq!(result["ok"], false);
    assert!(result["message"].as_str().unwrap().contains("P8"));
    assert!(!body.to_string().contains("acme-rotated"));

    // Ajout d'un client : dossier existant et sans chevauchement.
    let (_, fresh) = daemon.json("GET", "/api/tenants", None);
    let version = fresh["version"].as_str().unwrap().to_string();
    let beta_dir = fx.root.join("clients").join("beta");
    std::fs::create_dir_all(&beta_dir).unwrap();
    let overlap = json!({ "id": "beta", "dir": fx.acme.join("sub").to_string_lossy(), "version": version });
    std::fs::create_dir_all(fx.acme.join("sub")).unwrap();
    let (status, _) = daemon.json("POST", "/api/tenants", Some(&overlap));
    assert_eq!(status, 400);
    let add = json!({ "id": "beta", "dir": beta_dir.to_string_lossy(), "version": version });
    let (status, body) = daemon.json("POST", "/api/tenants", Some(&add));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(read_json(&projects_path)["projects"]["beta"]["tenant"], "beta");

    // Retrait : version exigée, périmée refusée avant la fenêtre ; sans fenêtre validée, rien n'est effacé.
    let (status, _) = daemon.json("DELETE", "/api/tenants/acme", None);
    assert_eq!(status, 400);
    let (status, _) = daemon.json("DELETE", "/api/tenants/acme?version=0-0", None);
    assert_eq!(status, 409);
    let (_, fresh) = daemon.json("GET", "/api/tenants", None);
    let version = fresh["version"].as_str().unwrap().to_string();
    let (status, body) = daemon.json("DELETE", &format!("/api/tenants/acme?version={}", version), None);
    assert_eq!(status, 403, "{}", body);
    assert_eq!(read_json(&projects_path)["projects"]["acme"]["tenant"], "acme");
    assert!(tenant_secrets.exists());

    // Audit : l'action et le nom du secret, jamais la valeur (écriture en tâche de fond).
    let audit = fx.hub.join("audit.jsonl");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&audit).unwrap_or_default();
        if text.contains("tenant.remove") {
            assert!(text.contains("tenant.secret_set ACME_TOKEN"));
            assert!(text.contains("tenant.servers_set"));
            assert!(!text.contains("acme-rotated"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "audit incomplet : {}", text);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn connexion_des_services_par_http() {
    let fx = Fixture::new("connect");
    let daemon = Daemon::start(&fx);
    let hub_secrets = fx.hub.join("secrets.env");
    let projects_path = fx.hub.join("projects.json");

    // Jeton du compte McpHub : seulement une variable attendue par un serveur de config.json.
    let (status, _) = daemon.raw(
        "PUT",
        "/api/secrets/HUB_TOKEN",
        Some(json!({ "value": "pirate" }).to_string()),
        Some(&daemon.token),
        Some("http://evil.example"),
    );
    assert_eq!(status, 403);
    let (status, text) = daemon.call("PUT", "/api/secrets/HUB_TOKEN", Some(&json!({ "value": "hub-rotated" })));
    assert_eq!(status, 200, "{}", text);
    assert!(!text.contains("hub-rotated"));
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["replaced"], true);
    assert_eq!(body["written"], json!(["HUB_TOKEN"]));
    assert!(std::fs::read_to_string(&hub_secrets).unwrap().contains("HUB_TOKEN=hub-rotated"));
    for name in ["NOT_USED", "MCPHUB_OAUTH_SVC", "bad-name"] {
        let (status, _) = daemon.call("PUT", &format!("/api/secrets/{}", name), Some(&json!({ "value": "x" })));
        assert_eq!(status, 400, "{}", name);
    }
    let (status, _) = daemon.call("PUT", "/api/secrets/HUB_TOKEN", Some(&json!({ "value": "  " })));
    assert_eq!(status, 400);

    // Cartes des serveurs : états des jetons, jamais de valeur.
    let (status, text) = daemon.call("GET", "/api/servers", None);
    assert_eq!(status, 200);
    assert!(!text.contains("hub-rotated") && !text.contains("hub-secret"));
    let servers: Value = serde_json::from_str(&text).unwrap();
    let svc = servers["servers"].as_array().unwrap().iter().find(|s| s["name"] == "svc").unwrap();
    assert_eq!(svc["keys"], json!([{ "var": "HUB_TOKEN", "present": true, "connector": null }]));
    assert_eq!(svc["oauth"]["capable"], false);

    // Adresse de retour OAuth collée : inconnue ou illisible, refusée.
    let (status, _) = daemon.call(
        "POST",
        "/api/oauth/manual",
        Some(&json!({ "url": "http://localhost:24680/oauth/callback?code=a&state=inconnu" })),
    );
    assert_eq!(status, 400);
    let (status, _) = daemon.call("POST", "/api/oauth/manual", Some(&json!({ "url": "pas une adresse" })));
    assert_eq!(status, 400);

    // Ouverture de page : HTTPS seulement.
    let (status, _) = daemon.call("POST", "/api/open", Some(&json!({ "url": "file:///etc/passwd" })));
    assert_eq!(status, 400);

    // Code d'appareil : réservé aux jetons GitHub attendus par le coffre.
    let (status, _) = daemon.call("POST", "/api/connect/device", Some(&json!({ "name": "ACME_TOKEN", "tenant": "acme" })));
    assert_eq!(status, 400);
    let (status, _) = daemon.call("GET", "/api/connect/device?id=inconnu", None);
    assert_eq!(status, 404);

    // Profil de navigateur du client : profil inconnu refusé, retour au navigateur par défaut.
    let (_, snapshot) = daemon.json("GET", "/api/tenants", None);
    assert_eq!(snapshot["tenants"][0]["browser"], Value::Null);
    let version = snapshot["version"].as_str().unwrap().to_string();
    let (status, _) = daemon.json(
        "PUT",
        "/api/tenants/acme/browser",
        Some(&json!({ "app": "Nope", "profile": "X", "version": version })),
    );
    assert_eq!(status, 400);
    let (status, body) = daemon.json("PUT", "/api/tenants/acme/browser", Some(&json!({ "version": version })));
    assert_eq!(status, 200, "{}", body);
    assert!(read_json(&projects_path)["projects"]["acme"].get("browser").is_none());

    // Échéance saisie à l'enregistrement : fiche sans valeur, alerte dans le tableau de bord.
    let (status, _) = daemon.call(
        "PUT",
        "/api/tenants/acme/secrets/ACME_TOKEN",
        Some(&json!({ "value": "acme-rotated", "expires_on": "31/12/2020" })),
    );
    assert_eq!(status, 400);
    let (status, text) = daemon.call(
        "PUT",
        "/api/tenants/acme/secrets/ACME_TOKEN",
        Some(&json!({ "value": "acme-rotated", "expires_on": "2020-12-31" })),
    );
    assert_eq!(status, 200, "{}", text);
    let meta = std::fs::read_to_string(fx.hub.join("tenants").join("acme").join("token-meta.json")).unwrap();
    assert!(!meta.contains("acme-rotated"));
    let (status, alerts) = daemon.json("GET", "/api/token-alerts", None);
    assert_eq!(status, 200);
    let alert = alerts["alerts"].as_array().unwrap().iter().find(|a| a["name"] == "ACME_TOKEN").expect("alerte");
    assert_eq!(alert["tenant"], "acme");
    assert_eq!(alert["level"], "expired");
    assert_eq!(alert["servers"], json!(["svc"]));
    assert!(alert["message"].as_str().unwrap().contains("expiré le 2020-12-31"));

    // Audit : le nom du secret, jamais la valeur.
    let audit = fx.hub.join("audit.jsonl");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&audit).unwrap_or_default();
        if text.contains("secret_set HUB_TOKEN") {
            assert!(!text.contains("hub-rotated"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "audit incomplet : {}", text);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn un_agent_apprend_qu_un_jeton_va_expirer() {
    let fx = Fixture::new("expiry");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let meta = json!({ "ACME_TOKEN": {
        "var": "TOKEN",
        "service": "github",
        "expires_at": now + 2 * 86_400_000 - 60_000,
        "servers": ["svc"]
    }});
    std::fs::write(fx.hub.join("tenants").join("acme").join("token-meta.json"), meta.to_string()).unwrap();
    let mut hub = fx.start();
    let acme_src = fx.acme.join("site").join("src");
    let call = |hub: &mut Hub, wd: Option<&Path>| {
        let mut args = json!({ "server": "svc", "tool": "get_identity", "arguments": {} });
        if let Some(w) = wd {
            args["_mcphub_context"] = json!({ "working_dir": w });
        }
        hub.request("tools/call", json!({ "name": "mcphub_call", "arguments": args })).to_string()
    };

    // Le compte McpHub n'est pas concerné par le jeton du client.
    assert!(!call(&mut hub, None).contains("expire dans"));
    // Depuis le dossier du client : l'agent lit quel jeton, quel compte, et où le renouveler.
    let first = call(&mut hub, Some(&acme_src));
    assert!(first.contains("token=acme-secret"), "{}", first);
    assert!(first.contains("GitHub ACME_TOKEN (client acme) : expire dans 2 jours"), "{}", first);
    assert!(first.contains("Client accounts, acme, secret ACME_TOKEN"), "{}", first);
    // Une fois par jour et par serveur, pas à chaque appel.
    assert!(!call(&mut hub, Some(&acme_src)).contains("expire dans"));
    hub.finish();
}
