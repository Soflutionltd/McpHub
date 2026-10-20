use serde_json::Value;
use std::io::Write;

use crate::redaction::redact_json_value;

pub fn run_export(raw: bool) {
    let path = dirs::home_dir()
        .unwrap_or_default()
        .join(".McpHub")
        .join("config.json");
    if let Ok(content) = std::fs::read_to_string(&path) {
        if raw {
            println!("{}", content);
            return;
        }

        match serde_json::from_str::<Value>(&content) {
            Ok(json) => {
                let redacted = redact_json_value(&json);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| "{}".to_string())
                );
            }
            Err(_) => {
                eprintln!("Failed to parse config.json");
            }
        }
    } else {
        eprintln!("Failed to read config.json");
    }
}

pub fn run_import(file: &str) {
    let dest = dirs::home_dir()
        .unwrap_or_default()
        .join(".McpHub")
        .join("config.json");
    if let Ok(content) = std::fs::read_to_string(file) {
        if let Ok(mut json) = serde_json::from_str::<Value>(&content) {
            let mut servers_opt = json.get_mut("mcpServers");
            if servers_opt.is_none() {
                servers_opt = json.get_mut("servers");
            }
            if let Some(servers_val) = servers_opt {
                if let Some(servers_obj) = servers_val.as_object_mut() {
                    for (name, srv) in servers_obj.iter_mut() {
                        if let Some(env) = srv.get_mut("env").and_then(|v| v.as_object_mut()) {
                            for (k, v) in env.iter_mut() {
                                if let Some(s) = v.as_str() {
                                    if s.is_empty()
                                        || s == "<your-token-here>"
                                        || s == "..."
                                        || s.starts_with('<')
                                    {
                                        print!("Enter value for {} (server {}): ", k, name);
                                        let _ = std::io::stdout().flush();
                                        let mut input = String::new();
                                        if std::io::stdin().read_line(&mut input).is_ok() {
                                            let trimmed = input.trim();
                                            if !trimmed.is_empty() {
                                                *v = serde_json::json!(trimmed);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&dest, serde_json::to_string_pretty(&json).unwrap()).is_ok() {
                println!("Imported successfully. Run 'McpHub generate' to rebuild cache.");
            } else {
                eprintln!("Failed to write to ~/.McpHub/config.json");
            }
        } else {
            eprintln!("Invalid JSON in {}", file);
        }
    } else {
        eprintln!("Failed to read file: {}", file);
    }
}
