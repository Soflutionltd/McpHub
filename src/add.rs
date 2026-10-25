use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

fn mcphub_dir() -> PathBuf {
    crate::instance::hub_dir()
}

pub async fn run() {
    println!("McpHub — Add Server");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    print!("Server name (e.g. github): ");
    io::stdout().flush().unwrap();
    let mut name = String::new();
    io::stdin().read_line(&mut name).unwrap();
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }

    print!("Command (e.g. npx, uvx, node): ");
    io::stdout().flush().unwrap();
    let mut command = String::new();
    io::stdin().read_line(&mut command).unwrap();
    let command = command.trim().to_string();
    if command.is_empty() {
        return;
    }

    print!("Arguments (space separated): ");
    io::stdout().flush().unwrap();
    let mut args_str = String::new();
    io::stdin().read_line(&mut args_str).unwrap();
    let args: Vec<String> = args_str.split_whitespace().map(|s| s.to_string()).collect();

    println!("Environment variables (KEY=VALUE, empty to finish):");
    let mut env = serde_json::Map::new();
    loop {
        print!("  > ");
        io::stdout().flush().unwrap();
        let mut kv = String::new();
        io::stdin().read_line(&mut kv).unwrap();
        let kv = kv.trim();
        if kv.is_empty() {
            break;
        }
        if let Some((k, v)) = kv.split_once('=') {
            env.insert(k.trim().to_string(), json!(v.trim()));
        }
    }

    println!("\nTesting connection... (simulated)");

    // Read existing config
    let path = mcphub_dir().join("config.json");
    let mut config: Value = if path.exists() {
        let content = fs::read_to_string(&path).unwrap();
        serde_json::from_str(&content).unwrap_or(json!({"mcpServers": {}}))
    } else {
        json!({"mcpServers": {}})
    };

    let key = if config.get("servers").is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    if config.get(key).is_none() {
        config[key] = json!({});
    }

    let servers = config.get_mut(key).unwrap().as_object_mut().unwrap();
    servers.insert(
        name.clone(),
        json!({
            "command": command,
            "args": args,
            "env": env
        }),
    );

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    println!("✓ Added '{}' to {}", name, path.display());
    println!("Run 'McpHub generate' to rebuild cache if needed.");
}
