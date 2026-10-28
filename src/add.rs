use serde_json::json;
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

    let path = mcphub_dir().join("config.json");
    let mut config = match crate::config::read_raw(&path) {
        Ok(c) => c.unwrap_or_else(|| json!({"mcpServers": {}})),
        Err(e) => {
            eprintln!("✗ {}", e);
            std::process::exit(1);
        }
    };

    let key = if config.get("servers").is_some() {
        "servers"
    } else {
        "mcpServers"
    };
    if config.get(key).is_none() {
        config[key] = json!({});
    }

    let Some(servers) = config.get_mut(key).and_then(|v| v.as_object_mut()) else {
        eprintln!("✗ {} : \"{}\" n'est pas un objet", path.display(), key);
        std::process::exit(1);
    };
    servers.insert(
        name.clone(),
        json!({
            "command": command,
            "args": args,
            "env": env
        }),
    );

    if let Err(e) = crate::config::write_raw_atomic(&path, &config) {
        eprintln!("✗ {}", e);
        std::process::exit(1);
    }

    println!("✓ Added '{}' to {}", name, path.display());
    println!("Run 'McpHub generate' to rebuild cache if needed.");
}
