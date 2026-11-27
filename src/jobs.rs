//! Appels longs : au-delà d'un délai (`settings.deferAfterSecs`, 45 s par défaut), McpHub
//! répond « toujours en cours » avec un numéro, et l'appel continue. L'agent relit le résultat
//! avec `mcphub_call server=mcphub tool=job_result {handle}`.
//!
//! Sans cela, un outil lent (un audit de dépendances, par exemple) dépasse le délai du client
//! (environ 60 s pour l'app Claude) : l'appel est perdu alors que le serveur travaillait encore.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Résultats terminés gardés ce temps, puis oubliés.
const KEEP_DONE: Duration = Duration::from_secs(30 * 60);
/// Nombre d'appels gardés au plus.
const MAX_JOBS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Running,
    Done(Result<serde_json::Value, String>),
}

#[derive(Debug, Clone)]
struct Job {
    /// Compte qui a lancé l'appel : seul ce compte relit le résultat.
    tenant: Option<String>,
    server: String,
    tool: String,
    started: Instant,
    finished: Option<Instant>,
    state: JobState,
}

fn jobs() -> std::sync::MutexGuard<'static, HashMap<String, Job>> {
    static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn new_handle() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Préfixe aléatoire : un numéro d'une ancienne session du démon ne désigne rien.
    let salt = crate::http_guard::random_token()
        .map(|t| t[..6].to_string())
        .unwrap_or_else(|| "000000".into());
    format!("job-{}-{}", salt, n)
}

/// Enregistre un appel encore en cours ; renvoie son numéro.
pub fn register(tenant: Option<String>, server: &str, tool: &str) -> String {
    let handle = new_handle();
    let mut table = jobs();
    let now = Instant::now();
    table.retain(|_, j| j.finished.is_none_or(|f| now.duration_since(f) < KEEP_DONE));
    if table.len() >= MAX_JOBS {
        // Les plus anciens terminés partent d'abord ; un appel en cours n'est jamais oublié.
        let mut done: Vec<(String, Instant)> = table
            .iter()
            .filter_map(|(h, j)| j.finished.map(|f| (h.clone(), f)))
            .collect();
        done.sort_by_key(|(_, f)| *f);
        for (h, _) in done.into_iter().take(table.len() + 1 - MAX_JOBS) {
            table.remove(&h);
        }
    }
    table.insert(
        handle.clone(),
        Job {
            tenant,
            server: server.to_string(),
            tool: tool.to_string(),
            started: now,
            finished: None,
            state: JobState::Running,
        },
    );
    handle
}

pub fn finish(handle: &str, result: Result<serde_json::Value, String>) {
    if let Some(job) = jobs().get_mut(handle) {
        job.finished = Some(Instant::now());
        job.state = JobState::Done(result);
    }
}

/// État d'un appel pour le compte qui le demande (`None` : inconnu, expiré ou d'un autre compte).
pub fn get(handle: &str, tenant: Option<&str>) -> Option<(String, String, u64, JobState)> {
    let table = jobs();
    let job = table.get(handle)?;
    if job.tenant.as_deref() != tenant {
        return None;
    }
    Some((
        job.server.clone(),
        job.tool.clone(),
        job.started.elapsed().as_secs(),
        job.state.clone(),
    ))
}

/// Réponse « toujours en cours » donnée à l'agent.
pub fn running_text(handle: &str, server: &str, tool: &str, seconds: u64) -> String {
    serde_json::json!({
        "status": "running",
        "handle": handle,
        "server": server,
        "tool": tool,
        "elapsed_secs": seconds,
        "note": "Still running in McpHub: the call was NOT lost and must NOT be sent again.",
        "next": format!("mcphub_call server=mcphub tool=job_result arguments={{\"handle\": \"{}\"}} in about 20 seconds.", handle),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_d_un_appel_long() {
        let h = register(None, "depsonar", "depsonar_audit");
        assert!(h.starts_with("job-"));
        assert_eq!(get(&h, None).map(|j| j.3), Some(JobState::Running));
        // Un autre compte ne voit rien.
        assert!(get(&h, Some("acme")).is_none());
        finish(&h, Ok(serde_json::json!({"content": []})));
        assert!(matches!(
            get(&h, None).map(|j| j.3),
            Some(JobState::Done(Ok(_)))
        ));
        assert!(get("job-inconnu-1", None).is_none());
        let text = running_text(&h, "depsonar", "depsonar_audit", 45);
        assert!(text.contains("must NOT be sent again") && text.contains(&h));
    }

    #[test]
    fn table_bornee_sans_oublier_un_appel_en_cours() {
        let running = register(Some("borne".into()), "s", "t");
        for _ in 0..(MAX_JOBS + 10) {
            let h = register(Some("borne".into()), "s", "t");
            finish(&h, Ok(serde_json::json!({})));
        }
        assert!(jobs().len() <= MAX_JOBS);
        assert_eq!(
            get(&running, Some("borne")).map(|j| j.3),
            Some(JobState::Running)
        );
    }
}
