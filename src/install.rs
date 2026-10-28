/// Cross-platform auto-start management.
/// `McpHub install`   — register McpHub to start at login
/// `McpHub uninstall` — remove auto-start
use std::fs;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn install() {
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("McpHub"));
    let binary_str = binary.display().to_string();
    let instance = crate::instance::current();
    let log_path = instance.hub_dir.join("mcphub.log");
    let port = instance.port;

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let plist_dir = home.join("Library/LaunchAgents");
        let _ = fs::create_dir_all(&plist_dir);
        let label = instance.launchd_label();
        let plist_path = plist_dir.join(format!("{}.plist", label));
        let env_entries: String = instance
            .service_env()
            .iter()
            .map(|(key, value)| format!("\n        <key>{}</key>\n        <string>{}</string>", key, xml_escape(value)))
            .collect();

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>EnvironmentVariables</key>
    <dict>{env_entries}
    </dict>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>"#,
            label = label,
            binary = xml_escape(&binary_str),
            env_entries = env_entries,
            log = xml_escape(&log_path.display().to_string()),
        );

        fs::write(&plist_path, &plist).expect("Failed to write LaunchAgent plist");

        // Load it
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.display().to_string()])
            .output();
        let output = std::process::Command::new("launchctl")
            .args(["load", &plist_path.display().to_string()])
            .output()
            .expect("Failed to run launchctl");

        if output.status.success() {
            let token = crate::dashboard::get_auth_token();
            println!("✓ McpHub installed as LaunchAgent");
            println!("  Plist: {}", plist_path.display());
            println!("  Log:   {}", log_path.display());
            println!("  McpHub will start automatically at login.");
            println!();
            println!("  Cursor config (~/.cursor/mcp.json):");
            println!("  {{");
            println!("    \"mcpServers\": {{");
            println!("      \"McpHub\": {{");
            println!("        \"url\": \"http://127.0.0.1:{}/sse\",", port);
            println!(
                "        \"headers\": {{\"Authorization\": \"Bearer {}\"}}",
                token
            );
            println!("      }}");
            println!("    }}");
            println!("  }}");
        } else {
            eprintln!(
                "✗ launchctl load failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let service_dir = home.join(".config/systemd/user");
        let _ = fs::create_dir_all(&service_dir);
        let unit = instance.systemd_unit();
        let service_path = service_dir.join(&unit);
        let env_lines: String = instance
            .service_env()
            .iter()
            .map(|(key, value)| format!("Environment=\"{}={}\"\n", key, value.replace('"', "\\\"")))
            .collect();

        let service = format!(
            r#"[Unit]
Description=McpHub MCP Proxy Server
After=network.target

[Service]
Type=simple
{env_lines}ExecStart={binary} serve
Restart=always
RestartSec=5
StandardError=append:{log}

[Install]
WantedBy=default.target"#,
            binary = binary_str,
            env_lines = env_lines,
            log = log_path.display(),
        );

        fs::write(&service_path, &service).expect("Failed to write systemd unit");

        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let output = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", &unit])
            .output()
            .expect("Failed to run systemctl");

        if output.status.success() {
            let token = crate::dashboard::get_auth_token();
            println!("✓ McpHub installed as systemd user service");
            println!("  Unit: {}", service_path.display());
            println!();
            println!("  Cursor config (~/.cursor/mcp.json):");
            println!("  {{");
            println!("    \"mcpServers\": {{");
            println!("      \"McpHub\": {{");
            println!("        \"url\": \"http://127.0.0.1:{}/sse\",", port);
            println!(
                "        \"headers\": {{\"Authorization\": \"Bearer {}\"}}",
                token
            );
            println!("      }}");
            println!("    }}");
            println!("  }}");
        } else {
            eprintln!(
                "✗ systemctl enable failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(name) = &instance.name {
            eprintln!(
                "✗ Auto-start of the named instance \"{}\" is not supported on Windows: run `McpHub serve` with MCPHUB_INSTANCE and MCPHUB_PORT set.",
                name
            );
            return;
        }
        // Windows: add to registry Run key
        let key_path = r"Software\Microsoft\Windows\CurrentVersion\Run";
        let output = std::process::Command::new("reg")
            .args([
                "add",
                &format!("HKCU\\{}", key_path),
                "/v",
                "McpHub",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{}\" serve", binary_str),
                "/f",
            ])
            .output()
            .expect("Failed to run reg");

        if output.status.success() {
            let token = crate::dashboard::get_auth_token();
            println!("✓ McpHub installed in Windows startup registry");
            println!();
            println!("  Cursor config (~/.cursor/mcp.json):");
            println!("  {{");
            println!("    \"mcpServers\": {{");
            println!("      \"McpHub\": {{");
            println!("        \"url\": \"http://127.0.0.1:{}/sse\",", port);
            println!(
                "        \"headers\": {{\"Authorization\": \"Bearer {}\"}}",
                token
            );
            println!("      }}");
            println!("    }}");
            println!("  }}");
        } else {
            eprintln!(
                "✗ Registry write failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

/// Démarrage automatique déjà enregistré pour l'instance courante.
pub fn is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|home| {
                home.join("Library/LaunchAgents")
                    .join(format!("{}.plist", crate::instance::current().launchd_label()))
                    .exists()
            })
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        dirs::home_dir()
            .map(|home| {
                home.join(".config/systemd/user")
                    .join(crate::instance::current().systemd_unit())
                    .exists()
            })
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("reg")
            .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "McpHub"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

pub fn uninstall() {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let plist_path = home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", crate::instance::current().launchd_label()));

        if plist_path.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_path.display().to_string()])
                .output();
            let _ = fs::remove_file(&plist_path);
            println!("✓ McpHub LaunchAgent removed");
        } else {
            println!("McpHub is not installed as a LaunchAgent");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let unit = crate::instance::current().systemd_unit();
        let service_path = home.join(".config/systemd/user").join(&unit);

        if service_path.exists() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", &unit])
                .output();
            let _ = fs::remove_file(&service_path);
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
            println!("✓ McpHub systemd service removed");
        } else {
            println!("McpHub is not installed as a systemd service");
        }
    }

    #[cfg(target_os = "windows")]
    {
        let key_path = r"Software\Microsoft\Windows\CurrentVersion\Run";
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                &format!("HKCU\\{}", key_path),
                "/v",
                "McpHub",
                "/f",
            ])
            .output();
        println!("✓ McpHub removed from Windows startup");
    }
}
