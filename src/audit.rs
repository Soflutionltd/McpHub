//! Journal d'audit : une ligne JSON par appel d'outil dans `~/.McpHub/audit.jsonl`.
//!
//! Chaque ligne : date, serveur, outil, décision de permission, statut, durée,
//! arguments masqués (secrets retirés) et erreur éventuelle. Rotation automatique.
//!
//! Configuration (`settings.audit` dans config.json) :
//! `{ "enabled": true, "logArguments": true, "maxSizeMb": 10, "keepFiles": 5 }`

use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ARGS_CHARS: usize = 4096;
const MAX_ERROR_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct AuditConfig {
    pub enabled: bool,
    pub log_arguments: bool,
    pub max_size_bytes: u64,
    pub keep_files: usize,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_arguments: true,
            max_size_bytes: 10 * 1024 * 1024,
            keep_files: 5,
        }
    }
}

impl AuditConfig {
    pub fn from_json(value: &Value) -> AuditConfig {
        let mut cfg = AuditConfig::default();
        if let Some(b) = value.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = b;
        }
        if let Some(b) = value.get("logArguments").and_then(|v| v.as_bool()) {
            cfg.log_arguments = b;
        }
        if let Some(mb) = value.get("maxSizeMb").and_then(|v| v.as_u64()) {
            cfg.max_size_bytes = mb.clamp(1, 1024) * 1024 * 1024;
        }
        if let Some(k) = value.get("keepFiles").and_then(|v| v.as_u64()) {
            cfg.keep_files = (k as usize).clamp(1, 50);
        }
        cfg
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuditEntry {
    pub server: String,
    pub tool: String,
    pub mode: &'static str,
    /// allow | confirm_requested | confirmed | deny | invalid_token
    pub decision: &'static str,
    /// rule#N | auto:destructive | default
    pub policy: String,
    /// ok | error | cache_hit | blocked | pending
    pub status: &'static str,
    pub duration_ms: Option<u64>,
    pub arguments: Option<Value>,
    pub error: Option<String>,
}

pub struct AuditLogger {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

pub fn default_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MCPHUB_HOME") {
        return PathBuf::from(dir).join("audit.jsonl");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".McpHub")
        .join("audit.jsonl")
}

impl AuditLogger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Écrit l'entrée en tâche de fond (n'attend jamais le disque).
    pub fn record(&self, cfg: &AuditConfig, entry: AuditEntry) {
        if !cfg.enabled {
            return;
        }
        let line = format_entry(cfg, &entry);
        let path = self.path.clone();
        let lock = self.lock.clone();
        let max = cfg.max_size_bytes;
        let keep = cfg.keep_files;
        let write = move || {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = append_line(&path, &line, max, keep) {
                eprintln!("[McpHub][WARN] audit: ecriture impossible: {}", e);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(write);
            }
            Err(_) => write(),
        }
    }
}

pub fn format_entry(cfg: &AuditConfig, e: &AuditEntry) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut obj = json!({
        "ts": iso_utc(now.as_secs(), now.subsec_millis()),
        "server": e.server,
        "tool": e.tool,
        "mode": e.mode,
        "decision": e.decision,
        "policy": e.policy,
        "status": e.status,
    });
    if let Some(d) = e.duration_ms {
        obj["duration_ms"] = json!(d);
    }
    if cfg.log_arguments {
        if let Some(args) = &e.arguments {
            obj["args"] = safe_arguments(args);
        }
    }
    if let Some(err) = &e.error {
        obj["error"] = json!(truncate(
            &crate::redaction::redact_text(err),
            MAX_ERROR_CHARS
        ));
    }
    obj.to_string()
}

/// Arguments sans secrets, et tronqués s'ils sont trop longs.
pub fn safe_arguments(args: &Value) -> Value {
    let redacted = crate::redaction::redact_json_value(args);
    let text = redacted.to_string();
    if text.chars().count() <= MAX_ARGS_CHARS {
        redacted
    } else {
        json!({ "_truncated": true, "preview": truncate(&text, MAX_ARGS_CHARS) })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn append_line(path: &PathBuf, line: &str, max: u64, keep: usize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= max {
            rotate(path, keep);
        }
    }
    let is_new = !path.exists();
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if is_new {
        crate::vault::restrict_permissions(path);
    }
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn rotated_name(path: &PathBuf, n: usize) -> PathBuf {
    path.with_extension(format!("{}.jsonl", n))
}

fn rotate(path: &PathBuf, keep: usize) {
    let _ = fs::remove_file(rotated_name(path, keep));
    for n in (1..keep).rev() {
        let from = rotated_name(path, n);
        if from.exists() {
            let _ = fs::rename(&from, rotated_name(path, n + 1));
        }
    }
    let _ = fs::rename(path, rotated_name(path, 1));
}

/// Date ISO 8601 UTC sans dépendance (algorithme de Howard Hinnant).
pub fn iso_utc(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, m, s, millis
    )
}

/// `McpHub audit [--tail N] [--server S] [--tool T] [--decision D] [--json]`
pub fn run_cli(args: &[String]) {
    let mut tail = 30usize;
    let mut server: Option<String> = None;
    let mut tool: Option<String> = None;
    let mut decision: Option<String> = None;
    let mut raw = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tail" | "-n" => {
                tail = it.next().and_then(|v| v.parse().ok()).unwrap_or(tail);
            }
            "--server" => server = it.next().cloned(),
            "--tool" => tool = it.next().cloned(),
            "--decision" => decision = it.next().cloned(),
            "--json" => raw = true,
            _ => {}
        }
    }
    let path = default_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            println!("Aucun journal d'audit pour l'instant ({}).", path.display());
            return;
        }
    };
    let matches = |v: &Value, key: &str, want: &Option<String>| match want {
        Some(w) => v
            .get(key)
            .and_then(|x| x.as_str())
            .map(|x| x.eq_ignore_ascii_case(w))
            .unwrap_or(false),
        None => true,
    };
    let rows: Vec<Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            matches(v, "server", &server)
                && matches(v, "tool", &tool)
                && matches(v, "decision", &decision)
        })
        .collect();
    let start = rows.len().saturating_sub(tail);
    for v in &rows[start..] {
        if raw {
            println!("{}", v);
            continue;
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let dur = v
            .get("duration_ms")
            .and_then(|x| x.as_u64())
            .map(|d| format!(" {}ms", d))
            .unwrap_or_default();
        let err = v
            .get("error")
            .and_then(|x| x.as_str())
            .map(|e| format!("  ! {}", e))
            .unwrap_or_default();
        println!(
            "{}  {:<9} {:<18} {}/{}{}{}",
            s("ts"),
            s("status"),
            s("decision"),
            s("server"),
            s("tool"),
            dur,
            err
        );
    }
    if rows.is_empty() {
        println!("Aucune entree ne correspond.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso_utc(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(iso_utc(1_792_454_400, 0), "2026-10-20T00:00:00.000Z");
    }

    #[test]
    fn secrets_are_redacted_and_long_args_truncated() {
        let args = json!({ "api_key": "sk_live_abcdefghijklmnop", "query": "facture" });
        let safe = safe_arguments(&args);
        assert_eq!(safe["query"], "facture");
        assert!(safe["api_key"].as_str().unwrap().starts_with("[REDACTED"));

        let big = json!({ "text": "x".repeat(10_000) });
        let safe = safe_arguments(&big);
        assert_eq!(safe["_truncated"], true);
    }

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("mcphub-audit-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        let cfg = AuditConfig {
            max_size_bytes: 200,
            keep_files: 2,
            ..AuditConfig::default()
        };
        for i in 0..20 {
            let entry = AuditEntry {
                server: "github".into(),
                tool: format!("tool_{}", i),
                mode: "discover",
                decision: "allow",
                policy: "default".into(),
                status: "ok",
                ..Default::default()
            };
            append_line(
                &path,
                &format_entry(&cfg, &entry),
                cfg.max_size_bytes,
                cfg.keep_files,
            )
            .unwrap();
        }
        assert!(path.exists());
        assert!(rotated_name(&path, 1).exists());
        assert!(rotated_name(&path, 2).exists());
        assert!(!rotated_name(&path, 3).exists());
        let last = fs::read_to_string(&path).unwrap();
        assert!(last.contains("tool_19"));
        let _ = fs::remove_dir_all(&dir);
    }
}
