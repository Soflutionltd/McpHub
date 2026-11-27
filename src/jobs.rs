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
    /// Processus appelé (`serveur` ou `serveur@tenant`) : avis de jeton à la relecture.
    process_key: String,
    /// Sortie d'un outil qui renvoie des données externes : marquée comme telle à la relecture.
    untrusted: bool,
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

/// Appel encore en cours à enregistrer.
pub struct NewJob {
    pub tenant: Option<String>,
    pub server: String,
    pub tool: String,
    pub process_key: String,
    pub untrusted: bool,
}

/// État d'un appel, vu par le compte qui l'a lancé.
pub struct JobView {
    pub server: String,
    pub tool: String,
    pub process_key: String,
    pub untrusted: bool,
    pub elapsed_secs: u64,
    pub state: JobState,
}

/// Enregistre un appel encore en cours ; renvoie son numéro.
pub fn register(new: NewJob) -> String {
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
            tenant: new.tenant,
            server: new.server,
            tool: new.tool,
            process_key: new.process_key,
            untrusted: new.untrusted,
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
pub fn get(handle: &str, tenant: Option<&str>) -> Option<JobView> {
    let table = jobs();
    let job = table.get(handle)?;
    if job.tenant.as_deref() != tenant {
        return None;
    }
    Some(JobView {
        server: job.server.clone(),
        tool: job.tool.clone(),
        process_key: job.process_key.clone(),
        untrusted: job.untrusted,
        elapsed_secs: job.started.elapsed().as_secs(),
        state: job.state.clone(),
    })
}

/// Réponse « toujours en cours » donnée à l'agent.
pub fn running_value(handle: &str, server: &str, tool: &str, seconds: u64) -> serde_json::Value {
    serde_json::json!({
        "status": "running",
        "handle": handle,
        "server": server,
        "tool": tool,
        "elapsed_secs": seconds,
        "note": "Still running in McpHub: the call was NOT lost and must NOT be sent again.",
        "next": format!("mcphub_call server=mcphub tool=job_result arguments={{\"handle\": \"{}\"}} in about 20 seconds.", handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_job(tenant: Option<&str>, server: &str, tool: &str) -> NewJob {
        NewJob {
            tenant: tenant.map(String::from),
            server: server.into(),
            tool: tool.into(),
            process_key: server.into(),
            untrusted: true,
        }
    }

    #[test]
    fn cycle_d_un_appel_long() {
        let h = register(new_job(None, "depsonar", "depsonar_audit"));
        assert!(h.starts_with("job-"));
        let view = get(&h, None).unwrap();
        assert_eq!(view.state, JobState::Running);
        assert!(
            view.untrusted,
            "la marque des donnees externes suit l'appel"
        );
        // Un autre compte ne voit rien.
        assert!(get(&h, Some("acme")).is_none());
        finish(&h, Ok(serde_json::json!({"content": []})));
        assert!(matches!(
            get(&h, None).map(|j| j.state),
            Some(JobState::Done(Ok(_)))
        ));
        assert!(get("job-inconnu-1", None).is_none());
        let text = running_value(&h, "depsonar", "depsonar_audit", 45).to_string();
        assert!(text.contains("must NOT be sent again") && text.contains(&h));
    }

    #[test]
    fn table_bornee_sans_oublier_un_appel_en_cours() {
        let running = register(new_job(Some("borne"), "s", "t"));
        for _ in 0..(MAX_JOBS + 10) {
            let h = register(new_job(Some("borne"), "s", "t"));
            finish(&h, Ok(serde_json::json!({})));
        }
        assert!(jobs().len() <= MAX_JOBS);
        assert_eq!(
            get(&running, Some("borne")).map(|j| j.state),
            Some(JobState::Running)
        );
    }
}
