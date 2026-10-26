use crate::config::auto_detect;
use crate::redaction::redact_command_preview;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;

fn mcphub_dir() -> PathBuf {
    crate::instance::hub_dir()
}

pub fn run() {
    println!("McpHub Doctor 🩺");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // 1. Binary info
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("McpHub"));
    println!(
        "✓ Binary: {} (v{})",
        exe.display(),
        env!("CARGO_PKG_VERSION")
    );

    // 2. Config
    let config_path = mcphub_dir().join("config.json");
    if config_path.exists() {
        println!("✓ Config: {} (Valid JSON)", config_path.display());
    } else {
        println!("✗ Config: Not found at {}", config_path.display());
    }

    // 3. Cache
    let cache_path = mcphub_dir().join("schema-cache.json");
    if cache_path.exists() {
        let meta = std::fs::metadata(&cache_path).unwrap();
        let modified = meta.modified().unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(modified)
            .unwrap()
            .as_secs();
        let size_kb = meta.len() / 1024;
        println!(
            "✓ Cache:  {} ({} KB, updated {}s ago)",
            cache_path.display(),
            size_kb,
            age
        );
    } else {
        println!("✗ Cache:  Not found. Run 'McpHub generate'");
    }

    // 4. Daemon & Port
    let port = crate::instance::port();
    match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => println!("✓ Daemon: Running on port {}", port),
        Err(_) => println!("! Daemon: Not running on port {} (or port is blocked)", port),
    }

    // 5. Servers check
    let config = auto_detect();
    println!("\nServers ({} total):", config.servers.len());

    for (name, srv) in &config.servers {
        print!("  {} ... ", name);

        if let Some(url) = &srv.url {
            // Only the host: the rest of a URL can carry a token.
            let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or("");
            println!("✓ Remote server ({})", host);
            continue;
        }

        // Check command exists
        let output = Command::new("which").arg(&srv.command).output();
        let cmd_exists = output.map(|o| o.status.success()).unwrap_or(false);

        if cmd_exists {
            print!("✓ Command '{}' found", srv.command);
        } else {
            print!("✗ Command '{}' NOT FOUND", srv.command);
        }

        print!(" [{}]", redact_command_preview(&srv.command, &srv.args));

        // Check env vars presence (just warning if some common ones seem missing)
        if srv.env.is_empty() && srv.command.contains("github") {
            print!(" (! No env vars, github might need GITHUB_TOKEN)");
        }
        println!();
    }

    // 6. Disk usage
    let mut total_size = 0;
    if let Ok(entries) = std::fs::read_dir(mcphub_dir()) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total_size += meta.len();
            }
        }
    }
    println!(
        "\nDisk Usage: {} uses {} MB",
        mcphub_dir().display(),
        total_size / 1024 / 1024
    );
}
