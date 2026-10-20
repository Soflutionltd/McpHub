use std::env;
use std::fs;
use std::process::Command;

const REPO: &str = "Soflution1/McpHub";

pub fn run() {
    let current_version = env!("CARGO_PKG_VERSION");
    println!("Checking for updates (current: v{})...", current_version);

    // 1. Get latest release from GitHub API using curl
    let output = Command::new("curl")
        .args([
            "-s",
            &format!("https://api.github.com/repos/{}/releases/latest", REPO),
        ])
        .output();

    let out = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => {
            eprintln!("Failed to check for updates. Is curl installed?");
            return;
        }
    };

    // Naive JSON parsing for "tag_name": "vX.Y.Z"
    let tag_line = out.lines().find(|l| l.contains("\"tag_name\""));
    let latest_version = if let Some(line) = tag_line {
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() >= 4 {
            parts[3].trim_start_matches('v').to_string()
        } else {
            eprintln!("Failed to parse version from GitHub API.");
            return;
        }
    } else {
        eprintln!("No release found on GitHub.");
        return;
    };

    if latest_version == current_version {
        println!("McpHub is up to date (v{}).", current_version);
        return;
    }

    println!("New version available: v{}! Downloading...", latest_version);

    // 2. Determine asset name
    let os = env::consts::OS;
    let arch = env::consts::ARCH;

    let asset_name = match (os, arch) {
        ("macos", "aarch64") => "McpHub-macos-arm.tar.gz",
        ("macos", "x86_64") => "McpHub-macos-intel.tar.gz",
        ("linux", "x86_64") => "McpHub-linux-amd64.tar.gz",
        ("linux", "aarch64") => "McpHub-linux-arm64.tar.gz",
        ("windows", "x86_64") => "McpHub-windows-amd64.zip",
        _ => {
            eprintln!("No pre-built binary available for {}/{}", os, arch);
            return;
        }
    };

    let download_url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        REPO, latest_version, asset_name
    );

    let temp_dir = env::temp_dir();
    let archive_path = temp_dir.join(asset_name);

    let dl_status = Command::new("curl")
        .args([
            "-L",
            "-s",
            "-o",
            archive_path.to_str().unwrap(),
            &download_url,
        ])
        .status();

    if !dl_status.map_or(false, |s| s.success()) {
        eprintln!("Download failed.");
        return;
    }

    // 3. Extract
    println!("Extracting...");
    let extract_status = Command::new("tar")
        .args([
            "-xf",
            archive_path.to_str().unwrap(),
            "-C",
            temp_dir.to_str().unwrap(),
        ])
        .status();

    if !extract_status.map_or(false, |s| s.success()) {
        eprintln!("Extraction failed. Is tar installed?");
        return;
    }

    let binary_name = if os == "windows" {
        "McpHub.exe"
    } else {
        "McpHub"
    };
    let new_binary_path = temp_dir.join(binary_name);

    if !new_binary_path.exists() {
        eprintln!("Extracted binary not found at {:?}", new_binary_path);
        return;
    }

    // 4. Replace current binary
    println!("Installing new binary...");
    let current_exe = match env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to get current executable path: {}", e);
            return;
        }
    };

    // Rename current to avoid "Text file busy" error on running daemon
    let old_exe = current_exe.with_extension("old");
    let _ = fs::rename(&current_exe, &old_exe);

    if let Err(e) = fs::copy(&new_binary_path, &current_exe) {
        eprintln!("Failed to replace binary: {}. Rolling back...", e);
        let _ = fs::rename(&old_exe, &current_exe);
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(mut perms) = fs::metadata(&current_exe).map(|m| m.permissions()) {
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&current_exe, perms);
        }
    }

    // Cleanup temp files
    let _ = fs::remove_file(archive_path);
    let _ = fs::remove_file(new_binary_path);
    let _ = fs::remove_file(old_exe);

    println!("Successfully updated to v{}!", latest_version);

    // 5. Restart daemon if installed
    println!("Restarting daemon to apply changes...");
    crate::install::install();

    println!("Update complete.");
}
