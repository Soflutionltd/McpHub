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
        let mut hub = Hub { child, stdin, stdout, next_id: 1 };
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
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Hub {
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{}", line).unwrap();
        self.stdin.flush().unwrap();
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
    drop(hub);

    // Journal d'audit : le tenant de chaque appel, jamais la valeur d'un secret.
    let audit = std::fs::read_to_string(fx.hub.join("audit.jsonl")).unwrap();
    let entries: Vec<Value> = audit.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    assert!(entries.iter().any(|e| e["tenant"] == "acme" && e["status"] == "ok" && e["server"] == "svc"));
    assert!(entries.iter().any(|e| e["tenant"] == "acme" && e["decision"] == "deny" && e["server"] == "other"));
    assert!(entries.iter().any(|e| e.get("tenant").is_none() && e["status"] == "ok"));
    assert!(!audit.contains("acme-secret") && !audit.contains("hub-secret"));
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
