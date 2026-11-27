//! Mesures par outil (appels, erreurs, p50 / p95) et écriture différée de `usage-stats.json`.
//!
//! - Les durées sont gardées sur une fenêtre bornée (les 200 derniers appels réels par outil) :
//!   la mémoire ne grandit pas avec l'ancienneté du démon.
//! - Une réponse servie par le cache est comptée à part (`cache_hits`) et n'entre pas dans les
//!   durées : elle ne fausse ni le p50 ni le p95.
//! - `usage-stats.json` n'est plus réécrit à chaque appel : au plus une écriture par
//!   intervalle, atomique, plus une à l'arrêt propre.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::ser::SerializeStruct;

/// Nombre de durées gardées par outil.
pub const LATENCY_WINDOW: usize = 200;
/// Intervalle minimal entre deux écritures de `usage-stats.json`.
pub const USAGE_WRITE_INTERVAL: Duration = Duration::from_secs(2);

/// Mesures d'un outil ("serveur__outil").
#[derive(Debug, Clone, Default)]
pub struct ToolMetrics {
    /// Appels réellement envoyés au serveur (réussis ou non).
    pub calls: u64,
    pub errors: u64,
    /// Réponses servies par le cache, hors `calls` et hors durées.
    pub cache_hits: u64,
    latencies_ms: VecDeque<u64>,
}

impl ToolMetrics {
    pub fn record_call(&mut self, elapsed_ms: u64, is_error: bool) {
        self.calls += 1;
        if is_error {
            self.errors += 1;
        }
        if self.latencies_ms.len() == LATENCY_WINDOW {
            self.latencies_ms.pop_front();
        }
        self.latencies_ms.push_back(elapsed_ms);
    }

    pub fn record_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    /// Centile par rang le plus proche sur la fenêtre ; `None` sans aucun appel réel.
    pub fn percentile_ms(&self, percent: u32) -> Option<u64> {
        if self.latencies_ms.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.latencies_ms.iter().copied().collect();
        sorted.sort_unstable();
        let rank = (sorted.len() * percent as usize).div_ceil(100).max(1);
        sorted.get(rank - 1).copied()
    }
}

impl serde::Serialize for ToolMetrics {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ToolMetrics", 6)?;
        state.serialize_field("calls", &self.calls)?;
        state.serialize_field("errors", &self.errors)?;
        state.serialize_field("cache_hits", &self.cache_hits)?;
        state.serialize_field("p50_ms", &self.percentile_ms(50))?;
        state.serialize_field("p95_ms", &self.percentile_ms(95))?;
        state.serialize_field("samples", &self.latencies_ms.len())?;
        state.end()
    }
}

#[derive(Default)]
struct UsageState {
    pending: Option<HashMap<String, u64>>,
    flush_scheduled: bool,
    writes: u64,
}

/// Écriture différée des compteurs d'usage : N appels rapprochés ne donnent qu'une écriture.
pub struct UsageWriter {
    path: PathBuf,
    interval: Duration,
    state: Mutex<UsageState>,
}

impl UsageWriter {
    pub fn new(path: PathBuf, interval: Duration) -> Self {
        Self {
            path,
            interval,
            state: Mutex::new(UsageState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, UsageState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retient le dernier état et programme une écriture dans `interval` s'il n'y en a pas
    /// déjà une en attente.
    pub fn schedule(self: &Arc<Self>, snapshot: HashMap<String, u64>) {
        {
            let mut state = self.state();
            state.pending = Some(snapshot);
            if state.flush_scheduled {
                return;
            }
            state.flush_scheduled = true;
        }
        let writer = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(writer.interval).await;
            writer.state().flush_scheduled = false;
            let flushed = tokio::task::spawn_blocking(move || writer.flush()).await;
            if let Err(e) = flushed {
                eprintln!(
                    "[McpHub][WARN] usage-stats.json : ecriture interrompue : {}",
                    e
                );
            }
        });
    }

    /// Écrit tout de suite ce qui attend (arrêt propre). Sans rien en attente : aucune écriture.
    pub fn flush(&self) {
        let Some(snapshot) = self.state().pending.take() else {
            return;
        };
        let written = serde_json::to_value(&snapshot)
            .map_err(|e| e.to_string())
            .and_then(|json| crate::config::write_raw_atomic(&self.path, &json));
        match written {
            Ok(()) => self.state().writes += 1,
            Err(e) => {
                eprintln!("[McpHub][WARN] usage-stats.json non ecrit : {}", e);
                // Rien n'est perdu : la prochaine écriture reprend cet état, sauf plus récent.
                self.state().pending.get_or_insert(snapshot);
            }
        }
    }

    #[cfg(test)]
    fn writes(&self) -> u64 {
        self.state().writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centiles_sur_une_serie_connue() {
        let mut m = ToolMetrics::default();
        assert_eq!(m.percentile_ms(50), None);
        // 1..=100 dans le desordre : p50 = 50, p95 = 95 (rang le plus proche).
        for v in (1..=100u64).rev() {
            m.record_call(v, false);
        }
        assert_eq!(m.percentile_ms(50), Some(50));
        assert_eq!(m.percentile_ms(95), Some(95));
        assert_eq!(m.percentile_ms(100), Some(100));

        let mut m = ToolMetrics::default();
        for v in [10, 20, 30, 40, 1000] {
            m.record_call(v, false);
        }
        assert_eq!(m.percentile_ms(50), Some(30));
        assert_eq!(m.percentile_ms(95), Some(1000));

        let mut m = ToolMetrics::default();
        m.record_call(7, true);
        assert_eq!(
            (m.percentile_ms(50), m.percentile_ms(95)),
            (Some(7), Some(7))
        );
    }

    #[test]
    fn fenetre_bornee_aux_200_dernieres_durees() {
        let mut m = ToolMetrics::default();
        for _ in 0..300 {
            m.record_call(5000, false);
        }
        for _ in 0..LATENCY_WINDOW {
            m.record_call(10, false);
        }
        assert_eq!(m.calls, 500);
        assert_eq!(m.latencies_ms.len(), LATENCY_WINDOW);
        assert_eq!(
            m.percentile_ms(95),
            Some(10),
            "les anciennes durees sont sorties de la fenetre"
        );
    }

    #[test]
    fn un_hit_de_cache_ne_fausse_pas_les_durees() {
        let mut m = ToolMetrics::default();
        m.record_call(100, false);
        m.record_call(300, true);
        for _ in 0..50 {
            m.record_cache_hit();
        }
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "calls": 2, "errors": 1, "cache_hits": 50,
                "p50_ms": 100, "p95_ms": 300, "samples": 2
            })
        );
        let only_hits = {
            let mut m = ToolMetrics::default();
            m.record_cache_hit();
            serde_json::to_value(&m).unwrap()
        };
        assert_eq!(only_hits["p50_ms"], serde_json::Value::Null);
    }

    fn temp_stats(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("mcphub-usage-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage-stats.json");
        (dir, path)
    }

    fn read_stats(path: &PathBuf) -> HashMap<String, u64> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn une_seule_ecriture_pour_des_appels_rapproches() {
        let (dir, path) = temp_stats("burst");
        let writer = Arc::new(UsageWriter::new(path.clone(), Duration::from_millis(150)));
        for n in 1..=20u64 {
            writer.schedule(HashMap::from([("s__get_x".to_string(), n)]));
        }
        assert_eq!(writer.writes(), 0, "rien n'est ecrit pendant la rafale");
        assert!(!path.exists());
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(writer.writes(), 1);
        assert_eq!(
            read_stats(&path)["s__get_x"],
            20,
            "le dernier etat est celui ecrit"
        );
        // Ecriture atomique : aucun fichier temporaire ne reste a cote.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // Une nouvelle rafale apres l'intervalle donne une nouvelle ecriture, pas plus.
        writer.schedule(HashMap::from([("s__get_x".to_string(), 21)]));
        writer.schedule(HashMap::from([("s__get_x".to_string(), 22)]));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(writer.writes(), 2);
        assert_eq!(read_stats(&path)["s__get_x"], 22);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn flush_a_l_arret_ecrit_ce_qui_attend() {
        let (dir, path) = temp_stats("flush");
        let writer = Arc::new(UsageWriter::new(path.clone(), Duration::from_secs(3600)));
        writer.flush();
        assert_eq!(writer.writes(), 0, "rien en attente : rien d'ecrit");
        assert!(!path.exists());
        writer.schedule(HashMap::from([("s__get_x".to_string(), 3)]));
        assert!(!path.exists(), "l'ecriture differee n'a pas encore eu lieu");
        writer.flush();
        assert_eq!(writer.writes(), 1);
        assert_eq!(read_stats(&path)["s__get_x"], 3);
        writer.flush();
        assert_eq!(
            writer.writes(),
            1,
            "un second flush sans nouveaute n'ecrit pas"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
