mod add;
pub mod auth;
mod benchmark;
mod cache;
pub mod child;
mod config;
mod dashboard;
mod doctor;
mod export;
mod health;
mod install;
mod log_store;
mod logs;
mod protocol;
mod proxy;
mod search;
mod sse;
mod update;

use config::auto_detect;
use proxy::ProxyServer;
use search::{IndexedTool, SearchEngine};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    eprintln!(
        r#"
McpHub v{VERSION} — Fastest MCP proxy with BM25 tool discovery

USAGE:
  McpHub              Start proxy (stdio + HTTP server on :24680)
  McpHub serve        Start HTTP-only server (SSE transport, no stdio)
  McpHub generate     Start all servers, index tools, save cache
  McpHub dashboard    Open web dashboard on http://127.0.0.1:24680
  McpHub install      Register McpHub to auto-start at login
  McpHub uninstall    Remove auto-start registration
  McpHub status       Show detected servers, cache, and health config
  McpHub doctor       Run full diagnostic of the installation
  McpHub logs         Tail daemon logs in real time
  McpHub add          Interactively add a new server
  McpHub benchmark    Measure start and ping times for servers
  McpHub export       Export configuration to stdout
  McpHub import       Import configuration from a file
  McpHub search "q"   Test BM25 search
  McpHub update       Self-update to the latest version on GitHub
  McpHub token list   List all API tokens
  McpHub token create <name> [server1,server2]  Create a new token (optionally restricted to servers)
  McpHub token revoke <token>                   Revoke a token
  McpHub version      Show version
  McpHub help         Show this help

TRANSPORT MODES:
  Default (stdio + HTTP):
    Cursor config: {{"mcpServers": {{"McpHub": {{"command": "/path/to/McpHub"}}}}}}
    Starts stdio proxy AND HTTP server on :24680 (dashboard + SSE)

  Serve (HTTP only, recommended):
    Cursor config: {{"mcpServers": {{"McpHub": {{"url": "http://127.0.0.1:24680/sse", "headers": {{"Authorization": "Bearer <token>"}}}}}}}}
    Run 'McpHub install' to auto-start, then configure Cursor with URL and token.
    Survives Cursor restarts. Single process for everything.

FIRST TIME SETUP:
  1. Configure servers in ~/.McpHub/config.json
  2. Run: McpHub generate    (one-time, ~60s)
  3. Run: McpHub install     (auto-start at login, prints auth token)
  4. Configure Cursor with URL and auth token
"#,
        VERSION = VERSION
    );
}

fn cmd_status() {
    let config = auto_detect();
    println!("McpHub v{}", VERSION);
    println!("Mode: {:?}", config.mode);
    println!("Servers configured: {}", config.servers.len());
    println!("Health: check={}s, auto_restart={}, notifications={}",
        config.health_check_interval_secs,
        config.health_auto_restart,
        config.health_notifications,
    );

    // Cache info
    if let Some(cached) = cache::load_cache() {
        let total_tools: usize = cached.servers.values().map(|v: &Vec<crate::protocol::ToolDef>| v.len()).sum::<usize>();
        println!("Cache: {} servers, {} tools (v{})", cached.servers.len(), total_tools, cached.version);
    } else {
        println!("Cache: NOT FOUND — run 'McpHub generate' first");
    }

    println!();
    let mut names: Vec<_> = config.servers.keys().collect();
    names.sort();
    for name in names {
        let s = &config.servers[name];
        let args = s.args.join(" ");
        println!("  {} → {} {}", name, s.command, args);
    }
}

async fn cmd_generate() {
    let config = auto_detect();
    if config.servers.is_empty() {
        eprintln!("No servers found. Add servers to ~/.McpHub/config.json");
        return;
    }

    let total = config.servers.len();
    eprintln!("Generating cache for {} servers...\n", total);

    let manager = std::sync::Arc::new(child::ChildManager::with_timeout(
        config.servers.clone(),
        config.idle_timeout_ms,
        config.request_timeout_secs,
    ));

    let mut server_tools: std::collections::HashMap<String, Vec<protocol::ToolDef>> = std::collections::HashMap::new();
    let mut server_errors: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut all_tools: Vec<IndexedTool> = Vec::new();
    let mut ok = 0;
    let mut fail = 0;

    let mut names: Vec<String> = config.servers.keys().cloned().collect();
    names.sort();

    for (i, name) in names.iter().enumerate() {
        eprint!("[{}/{}] {} ... ", i + 1, total, name);
        match manager.start_server(name).await {
            Ok(tools) => {
                eprintln!("{} tools ✓", tools.len());
                server_tools.insert(name.clone(), tools.clone());
                for tool in tools {
                    all_tools.push(IndexedTool {
                        name: format!("{}__{}", name, tool.name),
                        original_name: tool.name.clone(),
                        server_name: name.clone(),
                        description: tool.description.clone(),
                        tool_def: tool,
                    });
                }
                ok += 1;
            }
            Err(e) => {
                eprintln!("FAILED: {}", e);
                server_errors.insert(name.clone(), e);
                fail += 1;
            }
        }
    }

    // Build index to verify
    let mut engine = SearchEngine::new();
    engine.build_index(all_tools);

    // Save cache with errors
    cache::save_cache_with_errors(&server_tools, &server_errors);

    // Stop all servers
    manager.stop_all().await;

    eprintln!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("Done: {} OK, {} failed, {} total tools", ok, fail, engine.tool_count());
    eprintln!("Cache saved to ~/.McpHub/schema-cache.json");
    eprintln!("Proxy will now start instantly from cache.");
}

fn cmd_search(query: &str) {
    if let Some(cached) = cache::load_cache() {
        let mut engine = SearchEngine::new();
        let mut all_tools: Vec<IndexedTool> = Vec::new();
        for (server_name, tools) in &cached.servers {
            for tool in tools {
                all_tools.push(IndexedTool {
                    name: format!("{}__{}", server_name, tool.name),
                    original_name: tool.name.clone(),
                    server_name: server_name.to_string(),
                    description: tool.description.clone(),
                    tool_def: tool.clone(),
                });
            }
        }
        engine.build_index(all_tools);
        let results = engine.search(query, 10);
        println!("Query: \"{}\" ({} tools indexed)", query, engine.tool_count());
        for (i, t) in results.iter().enumerate() {
            println!("  {}. {} (server: {}) — {}", i + 1, t.original_name, t.server_name, &t.description[..t.description.len().min(80)]);
        }
    } else {
        println!("No cache found. Run 'McpHub generate' first.");
    }
}

/// HTTP-only server mode: dashboard + SSE, no stdio.
/// Used by `McpHub serve` and auto-start (install).
async fn cmd_serve() {
    eprintln!("McpHub v{} — serve mode (HTTP only)", VERSION);
    let config = auto_detect();
    let proxy = std::sync::Arc::new(ProxyServer::new(config));
    proxy.init().await;
    eprintln!("[McpHub][SERVE] Ready. Waiting for SSE connections on http://127.0.0.1:24680/sse");

    let proxy_shutdown = proxy.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        eprintln!("\n[McpHub] Shutting down gracefully...");
        proxy_shutdown.shutdown().await;
        std::process::exit(0);
    });

    dashboard::start_server(proxy).await;
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("help") | Some("--help") | Some("-h") => print_help(),
        Some("version") | Some("--version") | Some("-V") => println!("McpHub v{}", VERSION),
        Some("status") => cmd_status(),
        Some("doctor") => doctor::run(),
        Some("logs") => {
            let mut server = None;
            let mut level = None;
            let mut iter = args.iter().skip(2);
            while let Some(arg) = iter.next() {
                if arg == "--server" {
                    server = iter.next().map(|s| s.as_str());
                } else if arg == "--level" {
                    level = iter.next().map(|s| s.as_str());
                }
            }
            logs::run(server, level);
        }
        Some("add") => add::run().await,
        Some("benchmark") => benchmark::run().await,
        Some("export") => export::run_export(),
        Some("import") => {
            if let Some(file) = args.get(2) {
                export::run_import(file);
            } else {
                eprintln!("Usage: McpHub import <file>");
            }
        }
        Some("generate") => cmd_generate().await,
        Some("dashboard") | Some("ui") | Some("web") => dashboard::start_dashboard().await,
        Some("install") => install::install(),
        Some("uninstall") => install::uninstall(),
        Some("update") => update::run(),
        Some("serve") => cmd_serve().await,
        Some("search") => {
            let query = args.get(2).map(|s| s.as_str()).unwrap_or("*");
            cmd_search(query);
        }
        Some("token") => {
            match args.get(2).map(|s| s.as_str()) {
                Some("list") => {
                    let store = auth::load_tokens();
                    if store.tokens.is_empty() {
                        println!("No tokens configured. Using legacy auth-token file.");
                    } else {
                        println!("{:<50} {:<20} {}", "TOKEN", "NAME", "SERVERS");
                        println!("{}", "-".repeat(90));
                        let mut entries: Vec<_> = store.tokens.iter().collect();
                        entries.sort_by_key(|(_, e)| &e.name);
                        for (token, entry) in entries {
                            let servers = entry.allowed_servers.as_ref()
                                .map(|s| s.join(", "))
                                .unwrap_or_else(|| "all".into());
                            println!("{:<50} {:<20} {}", token, entry.name, servers);
                        }
                    }
                }
                Some("create") => {
                    let name = args.get(3).map(|s| s.as_str()).unwrap_or("unnamed");
                    let allowed = args.get(4).map(|s| {
                        s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>()
                    });
                    let token = auth::create_token(name, allowed);
                    println!("Created token for '{}':\n{}", name, token);
                }
                Some("revoke") => {
                    if let Some(token) = args.get(3) {
                        if auth::revoke_token(token) {
                            println!("Token revoked.");
                        } else {
                            eprintln!("Token not found.");
                            std::process::exit(1);
                        }
                    } else {
                        eprintln!("Usage: McpHub token revoke <token>");
                        std::process::exit(1);
                    }
                }
                _ => {
                    eprintln!("Usage: McpHub token list|create <name> [servers]|revoke <token>");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            // Default: stdio proxy + HTTP server with SSE
            eprintln!("McpHub v{} — starting...", VERSION);
            let config = auto_detect();
            let proxy = std::sync::Arc::new(ProxyServer::new(config));

            // Init proxy (load cache, start background tasks)
            proxy.init().await;

            let proxy_shutdown = proxy.clone();
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                #[cfg(not(unix))]
                {
                    tokio::signal::ctrl_c().await.ok();
                }
                eprintln!("\n[McpHub] Shutting down gracefully...");
                proxy_shutdown.shutdown().await;
                std::process::exit(0);
            });

            // Try to start HTTP server in background (non-blocking if port taken)
            let proxy_http = proxy.clone();
            tokio::spawn(async move {
                dashboard::start_server(proxy_http).await;
            });

            // Give HTTP server a moment to bind
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Run stdio loop (blocks until stdin closes)
            proxy.stdio_loop().await;
        }
    }
}
