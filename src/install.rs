/// Cross-platform auto-start management.
/// `McpHub install`   — register McpHub to start at login
/// `McpHub uninstall` — remove auto-start
use std::fs;
use std::path::PathBuf;

const LABEL: &str = "com.soflution.mcphub";

pub fn install() {
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("McpHub"));
    let binary_str = binary.display().to_string();

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let plist_dir = home.join("Library/LaunchAgents");
        let _ = fs::create_dir_all(&plist_dir);
        let plist_path = plist_dir.join(format!("{}.plist", LABEL));

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
    <key>StandardErrorPath</key>
    <string>{home}/.McpHub/mcphub.log</string>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>"#,
            label = LABEL,
            binary = binary_str,
            home = home.display(),
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
            println!("  Log:   ~/.McpHub/mcphub.log");
            println!("  McpHub will start automatically at login.");
            println!();
            println!("  Cursor config (~/.cursor/mcp.json):");
            println!("  {{");
            println!("    \"mcpServers\": {{");
            println!("      \"McpHub\": {{");
            println!("        \"url\": \"http://127.0.0.1:24680/sse\",");
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
        let service_path = service_dir.join("mcphub.service");

        let service = format!(
            r#"[Unit]
Description=McpHub MCP Proxy Server
After=network.target

[Service]
Type=simple
ExecStart={binary} serve
Restart=always
RestartSec=5
StandardError=append:{home}/.McpHub/mcphub.log

[Install]
WantedBy=default.target"#,
            binary = binary_str,
            home = home.display(),
        );

        fs::write(&service_path, &service).expect("Failed to write systemd unit");

        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let output = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "mcphub"])
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
            println!("        \"url\": \"http://127.0.0.1:24680/sse\",");
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
            println!("        \"url\": \"http://127.0.0.1:24680/sse\",");
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

pub fn uninstall() {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().expect("Cannot find home directory");
        let plist_path = home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", LABEL));

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
        let service_path = home.join(".config/systemd/user/mcphub.service");

        if service_path.exists() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "mcphub"])
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
