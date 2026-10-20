use crate::config::auto_detect;
use crate::proxy::{infer_discover_server_filter, DiscoverFilterSource};
use crate::search::{IndexedTool, SearchEngine};
use std::time::Instant;

pub async fn run() {
    let config = auto_detect();
    println!(
        "{:<15} | {:<10} | {:<10} | {:<8} | {:<8}",
        "Server", "Start", "Ping", "Tools", "RAM"
    );
    println!(
        "{:-<15}-|-{:-<10}-|-{:-<10}-|-{:-<8}-|-{:-<8}",
        "", "", "", "", ""
    );

    let manager = std::sync::Arc::new(crate::child::ChildManager::new(
        config.servers.clone(),
        300_000,
        config.request_timeout_secs,
    ));

    let mut names: Vec<_> = config.servers.keys().cloned().collect();
    names.sort();

    for name in names {
        let start_time = Instant::now();
        let tools_res = manager.start_server(&name).await;
        let start_duration = start_time.elapsed().as_millis();

        if let Ok(tools) = tools_res {
            let ping_start = Instant::now();
            let _ = manager
                .call_method(&name, "ping", serde_json::json!({}))
                .await;
            let ping_duration = ping_start.elapsed().as_millis();

            // Placeholder for RAM since accurate process tree measuring is complex in Rust without sysinfo crate
            let ram = "N/A";

            println!(
                "{:<15} | {:<8}ms | {:<8}ms | {:<8} | {:<8}",
                name,
                start_duration,
                ping_duration,
                tools.len(),
                ram
            );
        } else {
            println!(
                "{:<15} | {:<10} | {:<10} | {:<8} | {:<8}",
                name, "FAILED", "-", "-", "-"
            );
        }
    }

    manager.stop_all().await;

    if let Some(cached) = crate::cache::load_cache() {
        let mut engine = SearchEngine::new();
        let all_tools = cached
            .servers
            .iter()
            .flat_map(|(server_name, tools)| {
                let server_name = server_name.clone();
                tools.iter().cloned().map(move |tool| IndexedTool {
                    name: format!("{}__{}", server_name, tool.name),
                    original_name: tool.name.clone(),
                    server_name: server_name.clone(),
                    description: tool.description.clone(),
                    tool_def: tool,
                    untrusted_output: false,
                })
            })
            .collect();
        engine.build_index(all_tools);

        let mut server_names: Vec<String> = cached.servers.keys().cloned().collect();
        server_names.sort();
        let scenarios = build_routing_benchmark_scenarios(&cached.servers);

        println!();
        println!(
            "{:<28} | {:<23} | {:<14} | {:<7} | {:<7} | {:<7}",
            "Query", "Route", "Server", "CurHit", "BaseHit", "Ms"
        );
        println!(
            "{:-<28}-|-{:-<23}-|-{:-<14}-|-{:-<7}-|-{:-<7}-|-{:-<7}",
            "", "", "", "", "", ""
        );

        for scenario in scenarios {
            let route_start = Instant::now();
            let (server_filter, filter_source) =
                infer_discover_server_filter(&scenario.query, None, &server_names, &engine);
            let current_results = match server_filter.as_ref() {
                Some(server_name) => {
                    let allowed_servers = std::collections::HashSet::from([server_name.clone()]);
                    engine.search_filtered(&scenario.query, 5, Some(&allowed_servers))
                }
                None => engine.search(&scenario.query, 5),
            };
            let elapsed_ms = route_start.elapsed().as_secs_f64() * 1000.0;

            let baseline_results = engine.search(&scenario.query, 5);
            let current_hit = current_results
                .first()
                .map(|tool| tool.server_name == scenario.expected_server)
                .unwrap_or(false);
            let baseline_hit = baseline_results
                .first()
                .map(|tool| tool.server_name == scenario.expected_server)
                .unwrap_or(false);

            let route_label = benchmark_route_label(filter_source);
            println!(
                "{:<28} | {:<23} | {:<14} | {:<7} | {:<7} | {:<7.2}",
                truncate(&scenario.query, 28),
                truncate(&route_label, 23),
                truncate(&server_filter.unwrap_or_else(|| "-".to_string()), 14),
                yes_no(current_hit),
                yes_no(baseline_hit),
                elapsed_ms,
            );
        }
    }
}

struct RoutingBenchmarkScenario {
    query: String,
    expected_server: String,
}

fn build_routing_benchmark_scenarios(
    servers: &std::collections::HashMap<String, Vec<crate::protocol::ToolDef>>,
) -> Vec<RoutingBenchmarkScenario> {
    let mut tool_name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for tools in servers.values() {
        for tool in tools {
            *tool_name_counts.entry(tool.name.clone()).or_default() += 1;
        }
    }

    let mut names: Vec<String> = servers.keys().cloned().collect();
    names.sort();

    names
        .into_iter()
        .filter_map(|server_name| {
            servers.get(&server_name).and_then(|tools| {
                let representative_tool = tools
                    .iter()
                    .find(|tool| tool_name_counts.get(&tool.name).copied().unwrap_or(0) == 1)?;

                Some(RoutingBenchmarkScenario {
                    query: representative_tool.name.replace('_', " "),
                    expected_server: server_name,
                })
            })
        })
        .take(10)
        .collect()
}

fn benchmark_route_label(filter_source: Option<DiscoverFilterSource>) -> String {
    match filter_source {
        Some(DiscoverFilterSource::ExplicitServer) => "explicit + filtered_bm25".to_string(),
        Some(DiscoverFilterSource::QueryServerName) => "server_name + filtered_bm25".to_string(),
        Some(DiscoverFilterSource::QueryToolName) => "tool_name + filtered_bm25".to_string(),
        None => "plain_bm25".to_string(),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn truncate(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}
