use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::thread;
use std::time::Duration;

use crate::redaction::redact_text;

pub fn run(server_filter: Option<&str>, level_filter: Option<&str>) {
    let log_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".McpHub/mcphub.log");
    if !log_path.exists() {
        eprintln!("Log file not found at {}", log_path.display());
        return;
    }

    let file = File::open(&log_path).expect("Could not open log file");
    let mut reader = BufReader::new(file);

    // Seek to the end for a `tail -f` equivalent
    let mut pos = reader.seek(SeekFrom::End(0)).unwrap();

    println!("Tailing logs from {}...", log_path.display());
    if let Some(srv) = server_filter {
        println!("  Filter server: {}", srv);
    }
    if let Some(lvl) = level_filter {
        println!("  Filter level: {}", lvl);
    }

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF
                thread::sleep(Duration::from_millis(100));
                // reset EOF condition
                reader.seek(SeekFrom::Start(pos)).unwrap();
            }
            Ok(len) => {
                pos += len as u64;
                let line_trim = line.trim();

                // Filtering
                let mut show = true;
                if let Some(srv) = server_filter {
                    if !line_trim.contains(&format!("[{}]", srv)) && !line_trim.contains(srv) {
                        show = false;
                    }
                }
                if let Some(lvl) = level_filter {
                    let lvl_upper = lvl.to_uppercase();
                    if !line_trim.contains(&format!("[{}]", lvl_upper)) && !line_trim.contains(lvl)
                    {
                        show = false;
                    }
                }

                if show && !line_trim.is_empty() {
                    println!("{}", redact_text(line_trim));
                }
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
                reader.seek(SeekFrom::Start(pos)).unwrap();
            }
        }
    }
}
