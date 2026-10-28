use crate::config::Dedicated;
use crate::redaction::redact_command_preview;
use serde::Serialize;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;

fn mcphub_dir() -> PathBuf {
    crate::instance::hub_dir()
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    /// ok, warn ou error.
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct ServerCheck {
    name: String,
    kind: &'static str,
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct Report {
    version: &'static str,
    checks: Vec<Check>,
    servers: Vec<ServerCheck>,
    disk_bytes: u64,
}

/// `McpHub doctor [--json]` : code de sortie 1 si un contrôle est en erreur.
pub fn run(json: bool) {
    let report = collect();
    let failed = report.checks.iter().any(|c| c.status == "error")
        || report.servers.iter().any(|s| s.status == "error");
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        print_report(&report);
    }
    if failed {
        std::process::exit(1);
    }
}

fn collect() -> Report {
    let mut checks = Vec::new();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("McpHub"));
    checks.push(Check {
        name: "binary",
        status: "ok",
        detail: exe.display().to_string(),
    });

    let config_path = crate::config::dedicated_config_path();
    let config = match crate::config::load_dedicated() {
        Dedicated::Valid(c) => {
            checks.push(Check {
                name: "config",
                status: "ok",
                detail: format!("{} ({} servers)", config_path.display(), c.servers.len()),
            });
            Some(c)
        }
        Dedicated::Missing => {
            checks.push(Check {
                name: "config",
                status: "warn",
                detail: format!(
                    "{} not found: servers are imported from the editors' MCP configs",
                    config_path.display()
                ),
            });
            crate::config::try_auto_detect().ok()
        }
        Dedicated::Invalid(e) => {
            checks.push(Check {
                name: "config",
                status: "error",
                detail: format!("{} (the daemon keeps its last valid config, or starts with no server)", e),
            });
            None
        }
    };

    let cache_path = mcphub_dir().join("schema-cache.json");
    checks.push(match std::fs::metadata(&cache_path) {
        Ok(meta) => {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
                .map(|d| format!(", updated {}s ago", d.as_secs()))
                .unwrap_or_default();
            Check {
                name: "cache",
                status: "ok",
                detail: format!("{} ({} KB{})", cache_path.display(), meta.len() / 1024, age),
            }
        }
        Err(_) => Check {
            name: "cache",
            status: "warn",
            detail: "not found: run 'McpHub generate'".into(),
        },
    });

    let port = crate::instance::port();
    checks.push(match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => Check {
            name: "daemon",
            status: "ok",
            detail: format!("running on port {}", port),
        },
        Err(_) => Check {
            name: "daemon",
            status: "warn",
            detail: format!("not running on port {} (or the port is blocked)", port),
        },
    });

    let mut servers: Vec<ServerCheck> = config
        .map(|c| c.servers.into_iter().map(|(name, srv)| check_server(name, &srv)).collect())
        .unwrap_or_default();
    servers.sort_by(|a, b| a.name.cmp(&b.name));

    let disk_bytes = std::fs::read_dir(mcphub_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);

    Report {
        version: env!("CARGO_PKG_VERSION"),
        checks,
        servers,
        disk_bytes,
    }
}

fn check_server(name: String, srv: &crate::config::ServerConfig) -> ServerCheck {
    if let Some(url) = &srv.url {
        // Only the host: the rest of a URL can carry a token.
        let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or("");
        return ServerCheck {
            name,
            kind: "remote",
            status: "ok",
            detail: host.to_string(),
        };
    }
    let found = Command::new("which")
        .arg(&srv.command)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let preview = redact_command_preview(&srv.command, &srv.args);
    ServerCheck {
        name,
        kind: "local",
        status: if found { "ok" } else { "error" },
        detail: if found {
            preview
        } else {
            format!("command '{}' not found [{}]", srv.command, preview)
        },
    }
}

fn print_report(report: &Report) {
    println!("McpHub Doctor 🩺 (v{})", report.version);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let mark = |status: &str| match status {
        "ok" => "✓",
        "warn" => "!",
        _ => "✗",
    };
    for check in &report.checks {
        println!("{} {:<7} {}", mark(check.status), check.name, check.detail);
    }
    println!("\nServers ({} total):", report.servers.len());
    for server in &report.servers {
        println!("  {} {} ({}) {}", mark(server.status), server.name, server.kind, server.detail);
    }
    println!(
        "\nDisk Usage: {} uses {} MB",
        mcphub_dir().display(),
        report.disk_bytes / 1024 / 1024
    );
}
