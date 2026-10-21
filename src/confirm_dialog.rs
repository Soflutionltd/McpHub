//! Confirmation humaine : fenêtre macOS native (Autoriser / Refuser) ouverte par le démon.
//!
//! Pourquoi : avec le jeton de confirmation, c'est l'agent qui rappelle l'outil avec le jeton ;
//! un agent mal réglé peut le faire sans avoir demandé. Ici seul un humain devant l'écran peut
//! cliquer "Autoriser". Le texte passe en argument d'osascript (`argv`), jamais interpolé dans
//! le script : pas d'injection AppleScript possible depuis les arguments d'un outil.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAnswer {
    Allow,
    Deny,
    Timeout,
    /// Pas d'interface graphique ou osascript absent : repli sur le jeton.
    Unavailable,
}

fn shorten(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str(" [...]");
    }
    out
}

const PREVIEW_MAX: usize = 900;

/// Texte de la fenêtre. Si les arguments sont trop longs pour être affichés en entier, on le
/// dit clairement et on liste toutes les clés : un argument dangereux ne peut pas se cacher
/// derrière un remplissage.
pub fn build_message(server: &str, tool: &str, reason: &str, args: &serde_json::Value, origin: &str) -> String {
    let pretty = serde_json::to_string_pretty(args).unwrap_or_default();
    let total = pretty.chars().count();
    let args_block = if total > PREVIEW_MAX {
        let keys: Vec<String> = args
            .as_object()
            .map(|o| o.keys().map(|k| shorten(k, 40)).collect())
            .unwrap_or_default();
        format!(
            "ATTENTION : arguments trop longs pour etre affiches en entier ({} caracteres, seuls les {} premiers sont visibles).\nCles : {}\n\n{}",
            total,
            PREVIEW_MAX,
            shorten(&keys.join(", "), 300),
            shorten(&pretty, PREVIEW_MAX)
        )
    } else {
        pretty
    };
    format!(
        "Une IA veut executer une action sensible.\n\nServeur : {}\nOutil : {}\nMotif : {}\nDepuis : {}\n\nArguments :\n{}\n\nAutoriser cette action une seule fois ?",
        shorten(server, 80),
        shorten(tool, 120),
        shorten(reason, 160),
        shorten(origin, 80),
        args_block
    )
}

/// Bloquant : à appeler via `spawn_blocking`.
pub fn ask_blocking(message: &str, timeout_secs: u64) -> DialogAnswer {
    if !cfg!(target_os = "macos") || std::env::var_os("MCPHUB_NO_DIALOG").is_some() {
        return DialogAnswer::Unavailable;
    }
    let script = [
        "on run argv",
        "tell current application to activate",
        &format!(
            "set r to display dialog (item 1 of argv) with title \"McpHub : confirmation\" buttons {{\"Refuser\", \"Autoriser\"}} default button \"Refuser\" cancel button \"Refuser\" giving up after {} with icon caution",
            timeout_secs.clamp(10, 600)
        ),
        "if gave up of r then return \"timeout\"",
        "return button returned of r",
        "end run",
    ];
    let mut cmd = Command::new("/usr/bin/osascript");
    for line in script {
        cmd.arg("-e").arg(line);
    }
    cmd.arg("--").arg(message);
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() {
                match stdout.as_str() {
                    "Autoriser" => DialogAnswer::Allow,
                    "timeout" => DialogAnswer::Timeout,
                    _ => DialogAnswer::Deny,
                }
            } else if stderr.contains("-128") {
                // Bouton d'annulation "Refuser" ou touche Échap.
                DialogAnswer::Deny
            } else {
                eprintln!("[McpHub][CONFIRM] Fenetre indisponible : {}", stderr.trim());
                DialogAnswer::Unavailable
            }
        }
        Err(e) => {
            eprintln!("[McpHub][CONFIRM] osascript introuvable : {}", e);
            DialogAnswer::Unavailable
        }
    }
}

/// Une seule fenêtre à la fois : les demandes simultanées attendent leur tour.
pub async fn ask(message: String, timeout_secs: u64) -> DialogAnswer {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    // File d'attente bornée : au-delà du délai, la demande est abandonnée (refus).
    let Ok(_guard) =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), LOCK.lock()).await
    else {
        return DialogAnswer::Timeout;
    };
    tokio::task::spawn_blocking(move || ask_blocking(&message, timeout_secs))
        .await
        .unwrap_or(DialogAnswer::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_borne_et_lisible() {
        let args = serde_json::json!({"aaa": "x".repeat(5000), "drop": "production"});
        let m = build_message("supabase", "delete_branch", "nom destructeur", &args, "Cursor");
        assert!(m.contains("supabase") && m.contains("delete_branch"));
        assert!(m.len() < 2200);
        assert!(m.contains("ATTENTION"));
        assert!(m.contains("aaa, drop"), "toutes les cles visibles");
        let short = build_message("s", "t", "r", &serde_json::json!({"id": 1}), "o");
        assert!(!short.contains("ATTENTION"));
    }
}
