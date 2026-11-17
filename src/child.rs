/// Child process manager: spawn MCP servers, communicate over stdio, manage lifecycle.
///
/// Verrous : la table globale des pools n'est jamais tenue pendant une attente (appel, ping,
/// verrou d'un processus). Chaque processus local a son propre verrou, tenu le temps d'un
/// échange. Un serveur distant n'en a pas : ses requêtes HTTP partent en parallèle.
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub use crate::config::ServerConfig;
use crate::protocol::ToolDef;
use crate::redaction::redact_text;

/// Préfixe d'une erreur où la requête n'a jamais atteint le serveur : la relancer est sans risque.
pub const NOT_SENT: &str = "[non transmis]";
/// Préfixe d'une erreur où la requête a été transmise sans réponse : l'outil a pu s'exécuter.
pub const UNKNOWN_OUTCOME: &str = "[issue inconnue]";

pub fn is_unknown_outcome(e: &str) -> bool {
    e.starts_with(UNKNOWN_OUTCOME)
}

pub fn is_not_sent(e: &str) -> bool {
    e.starts_with(NOT_SENT)
}

/// Échec d'un échange JSON-RPC, classé selon ce que le serveur a pu recevoir.
#[derive(Debug)]
enum CallError {
    /// Écriture impossible : le processus est mort, rien n'a été reçu.
    Dead(String),
    /// Le processus était occupé jusqu'à l'échéance : rien n'a été envoyé.
    Busy(String),
    /// Connexion perdue après l'envoi : l'effet est inconnu.
    Lost(String),
    /// Pas de réponse avant l'échéance après l'envoi : l'effet est inconnu.
    NoReply(String),
    /// Réponse d'erreur du serveur (JSON-RPC ou transport distant).
    Server(String),
}

impl CallError {
    fn into_message(self) -> String {
        match self {
            CallError::Dead(e) | CallError::Busy(e) => format!("{} {}", NOT_SENT, e),
            CallError::Lost(e) | CallError::NoReply(e) => format!("{} {}", UNKNOWN_OUTCOME, e),
            CallError::Server(e) => e,
        }
    }
}

struct StdioProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    server_name: String,
}

impl StdioProcess {
    async fn kill(&mut self) {
        if let Some(pid) = self.child.id() {
            kill_group(pid);
        }
        let _ = self.child.kill().await;
    }

    fn exited(&mut self) -> Result<Option<String>, String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(status.to_string())),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

enum Link {
    Stdio(Mutex<StdioProcess>),
    Http(Arc<crate::remote::RemoteClient>),
}

/// Une instance du pool et le nombre d'appels qu'elle traite en ce moment.
struct Instance {
    link: Link,
    in_flight: AtomicUsize,
}

/// Compte un appel en cours, y compris si son futur est abandonné (délai dépassé).
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Instance {
    fn new(link: Link) -> Self {
        Self { link, in_flight: AtomicUsize::new(0) }
    }

    fn busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    async fn kill(&self) {
        match &self.link {
            Link::Stdio(proc) => proc.lock().await.kill().await,
            Link::Http(remote) => remote.close().await,
        }
    }

    /// Un échange JSON-RPC. Le verrou d'un processus local est tenu pendant tout l'échange
    /// (une seule lecture de stdout à la fois) ; un serveur distant n'attend aucun verrou.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        sent: &AtomicBool,
    ) -> Result<serde_json::Value, CallError> {
        match &self.link {
            Link::Http(remote) => {
                sent.store(true, Ordering::SeqCst);
                remote.request(method, params).await.map_err(CallError::Server)
            }
            Link::Stdio(proc) => {
                let mut proc = proc.lock().await;
                stdio_request(&mut proc, method, params, sent).await
            }
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        match &self.link {
            Link::Http(remote) => remote.notify(method, params).await,
            Link::Stdio(proc) => {
                let mut proc = proc.lock().await;
                stdio_notify(&mut proc, method, params).await
            }
        }
    }
}

/// De quoi arrêter un processus sans prendre son verrou (tenu pendant un appel en cours).
enum KillHandle {
    Pid(u32),
    Remote(Arc<crate::remote::RemoteClient>),
}

/// Tue le groupe de processus entier : `npx` et le `node` qu'il a lancé, par exemple.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: killpg n'accède à aucune mémoire ; un groupe déjà disparu renvoie ESRCH.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct ServerPool {
    procs: Vec<Arc<Instance>>,
    handles: Vec<KillHandle>,
    tools: Vec<ToolDef>,
    request_timeout_secs: u64,
    next_idx: AtomicUsize,
    last_used_ms: AtomicU64,
}

impl ServerPool {
    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)))
    }

    fn pick(&self) -> Arc<Instance> {
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed) % self.procs.len();
        self.procs[idx].clone()
    }

    fn busy(&self) -> bool {
        self.procs.iter().any(|p| p.busy())
    }

    async fn kill_now(&self) {
        for handle in &self.handles {
            match handle {
                KillHandle::Pid(pid) => kill_group(*pid),
                KillHandle::Remote(remote) => remote.close().await,
            }
        }
    }
}

/// Variables système transmises à tous les serveurs.
const BASE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
    "TERM", "__CF_USER_TEXT_ENCODING", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy",
    "https_proxy", "no_proxy", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "TZ", "NVM_DIR",
    "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
];

pub fn base_child_env(unsandboxed: bool, inherit: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in BASE_ENV {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    // L'agent SSH permet de signer avec les clés : réservé aux serveurs non isolés.
    if unsandboxed {
        if let Ok(v) = std::env::var("SSH_AUTH_SOCK") {
            out.push(("SSH_AUTH_SOCK".into(), v));
        }
    }
    for k in inherit {
        if let Ok(v) = std::env::var(k) {
            out.push((k.clone(), v));
        }
    }
    out
}

/// Commande d'un serveur local, prête à lancer : sandbox, environnement minimal (jamais les
/// secrets chargés dans le démon), ses propres variables, groupe de processus dédié.
/// Utilisée par le démarrage normal et par le diagnostic du tableau de bord.
pub fn local_command(name: &str, config: &ServerConfig) -> Command {
    let (program, program_args) = crate::sandbox::wrap(
        name,
        config.sandbox.as_ref(),
        &config.command,
        &config.args,
        &config.env,
    );
    let mut cmd = Command::new(&program);
    cmd.args(&program_args).kill_on_drop(true);
    cmd.env_clear();
    for (k, v) in base_child_env(config.sandbox.is_none(), &config.inherit_env) {
        cmd.env(k, v);
    }
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Sortie d'erreur d'un serveur enfant : ~/Library/Logs/McpHub/<serveur>.stderr.log (remise à zéro
/// au-delà de 1 Mo). Auparavant jetée, ce qui rendait les pannes de démarrage illisibles.
fn child_stderr(name: &str) -> Stdio {
    if cfg!(test) {
        return Stdio::null();
    }
    let Some(home) = dirs::home_dir() else {
        return Stdio::null();
    };
    // Hors de ~/.McpHub : ce dossier est interdit aux serveurs isolés, et un serveur dont la
    // sortie d'erreur pointe vers un fichier interdit plante au démarrage (Node).
    let dir = home.join("Library").join("Logs").join("McpHub");
    if std::fs::create_dir_all(&dir).is_err() {
        return Stdio::null();
    }
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("{}.stderr.log", safe));
    let too_big = std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    }
}

/// Après un démarrage raté, les appels qui attendaient reçoivent la même erreur pendant ce délai
/// au lieu de relancer chacun trois tentatives.
const START_FAILURE_MEMORY: Duration = Duration::from_secs(5);

pub struct ChildManager {
    configs: Arc<Mutex<HashMap<String, ServerConfig>>>,
    pools: Arc<Mutex<HashMap<String, Arc<ServerPool>>>>,
    /// Un verrou de démarrage par serveur : vingt premières requêtes = un seul démarrage.
    starting: Mutex<HashMap<String, Arc<Mutex<Option<(Instant, String)>>>>>,
    idle_timeout_ms: u64,
    request_timeout_secs: u64,
    /// Augmente à chaque serveur perdu, relancé ou arrêté : les réponses mises en cache avant
    /// ne sont plus servies (l'état du serveur a pu changer).
    epoch: AtomicU64,
}

impl ChildManager {
    pub fn new(
        configs: HashMap<String, ServerConfig>,
        idle_timeout_ms: u64,
        request_timeout_secs: u64,
    ) -> Self {
        Self {
            configs: Arc::new(Mutex::new(configs)),
            pools: Arc::new(Mutex::new(HashMap::new())),
            starting: Mutex::new(HashMap::new()),
            idle_timeout_ms,
            request_timeout_secs: request_timeout_secs.max(1),
            epoch: AtomicU64::new(0),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub async fn update_configs(&self, new_configs: HashMap<String, ServerConfig>) {
        let to_stop: Vec<String> = {
            let mut current_configs = self.configs.lock().await;
            let changed = current_configs
                .iter()
                .filter(|(name, old_cfg)| new_configs.get(*name) != Some(*old_cfg))
                .map(|(name, _)| name.clone())
                .collect();
            *current_configs = new_configs;
            changed
        };
        for name in to_stop {
            self.stop_server(&name).await;
        }
    }

    /// Nom canonique (clé de config) d'un serveur. Utilisé aussi par le proxy pour que
    /// les permissions s'appliquent exactement au serveur qui sera appelé.
    pub async fn resolve_name(&self, name: &str) -> Option<String> {
        let configs = self.configs.lock().await;
        if configs.contains_key(name) {
            return Some(name.to_string());
        }
        let lower = name.to_lowercase();
        for key in configs.keys() {
            if key.to_lowercase() == lower {
                return Some(key.clone());
            }
        }
        let normalized = lower.replace(['-', '_'], "");
        for key in configs.keys() {
            let key_normalized = key.to_lowercase().replace(['-', '_'], "");
            if key_normalized == normalized {
                return Some(key.clone());
            }
        }
        None
    }

    async fn pool(&self, name: &str) -> Option<Arc<ServerPool>> {
        self.pools.lock().await.get(name).cloned()
    }

    /// Retire le pool de la table seulement s'il n'a pas déjà été remplacé (redémarrage concurrent).
    async fn remove_if_same(&self, name: &str, pool: &Arc<ServerPool>) -> bool {
        let mut pools = self.pools.lock().await;
        match pools.get(name) {
            Some(current) if Arc::ptr_eq(current, pool) => {
                pools.remove(name);
                true
            }
            _ => false,
        }
    }

    async fn discard(&self, name: &str, pool: &Arc<ServerPool>) {
        if self.remove_if_same(name, pool).await {
            self.bump_epoch();
            pool.kill_now().await;
        }
    }

    pub async fn start_server(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let name_resolved = self
            .resolve_name(name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", name))?;
        let name = name_resolved.as_str();

        if let Some(pool) = self.pool(name).await {
            pool.touch();
            return Ok(pool.tools.clone());
        }

        let gate = {
            let mut starting = self.starting.lock().await;
            starting.entry(name.to_string()).or_default().clone()
        };
        let mut last_failure = gate.lock().await;
        if let Some(pool) = self.pool(name).await {
            pool.touch();
            return Ok(pool.tools.clone());
        }
        if let Some((at, error)) = last_failure.as_ref() {
            if at.elapsed() < START_FAILURE_MEMORY {
                return Err(error.clone());
            }
        }

        const MAX_RETRIES: u32 = 3;
        const BACKOFF_MS: [u64; 3] = [500, 1000, 2000];
        let mut last_error = String::new();

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = BACKOFF_MS
                    .get(attempt as usize - 1)
                    .copied()
                    .unwrap_or(2000);
                eprintln!(
                    "[McpHub][RETRY] {} attempt {}/{} (backoff {}ms)",
                    name,
                    attempt + 1,
                    MAX_RETRIES,
                    delay
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match self.try_start_pool(name).await {
                Ok(tools) => {
                    *last_failure = None;
                    return Ok(tools);
                }
                Err(e) => {
                    last_error = e;
                    if attempt < MAX_RETRIES - 1 {
                        eprintln!(
                            "[McpHub][WARN] {} failed: {} — retrying...",
                            name, last_error
                        );
                    }
                }
            }
        }

        let error = format!("{} (after {} attempts)", last_error, MAX_RETRIES);
        *last_failure = Some((Instant::now(), error.clone()));
        Err(error)
    }

    async fn try_start_pool(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let config = {
            let configs = self.configs.lock().await;
            configs
                .get(name)
                .ok_or_else(|| format!("Unknown server: {}", name))?
                .clone()
        };
        let request_timeout_secs = config
            .request_timeout_secs
            .unwrap_or(self.request_timeout_secs)
            .max(1);

        let pool_size = config.pool.max(1);
        let mut procs: Vec<Arc<Instance>> = Vec::new();
        let mut handles = Vec::new();
        let mut first_tools = Vec::new();

        for i in 0..pool_size {
            match start_instance(name, &config, i, pool_size, request_timeout_secs).await {
                Ok((proc, handle, tools)) => {
                    if i == 0 {
                        first_tools = tools;
                    }
                    procs.push(Arc::new(proc));
                    handles.push(handle);
                }
                Err(e) => {
                    for p in &procs {
                        p.kill().await;
                    }
                    return Err(e);
                }
            }
        }

        let pool = Arc::new(ServerPool {
            procs,
            handles,
            tools: first_tools.clone(),
            request_timeout_secs,
            next_idx: AtomicUsize::new(0),
            last_used_ms: AtomicU64::new(now_ms()),
        });

        let previous = self.pools.lock().await.insert(name.to_string(), pool);
        if let Some(previous) = previous {
            previous.kill_now().await;
        }

        Ok(first_tools)
    }

    /// Un échange sur une instance du pool, sous une seule échéance qui couvre aussi
    /// l'attente du verrou (un processus local occupé par un appel précédent).
    async fn exchange(
        &self,
        pool: &ServerPool,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        pool.touch();
        let instance = pool.pick();
        let _in_flight = InFlight::enter(&instance.in_flight);
        let secs = pool.request_timeout_secs;
        let sent = AtomicBool::new(false);
        let outcome = tokio::time::timeout(
            Duration::from_secs(secs),
            instance.request(method, params, &sent),
        )
        .await;
        pool.touch();
        match outcome {
            Ok(result) => result,
            Err(_) if sent.load(Ordering::SeqCst) => Err(CallError::NoReply(format!(
                "Timeout: server did not respond within {}s",
                secs
            ))),
            Err(_) => Err(CallError::Busy(format!(
                "Timeout: server busy for {}s, request not sent",
                secs
            ))),
        }
    }

    /// Méthodes de lecture (prompts/get, resources/read, ping) : un nouvel essai après une
    /// coupure est sans effet de bord.
    pub async fn call_method(
        &self,
        server_name: &str,
        method: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resolved = self
            .resolve_name(server_name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        let pool = self
            .pool(server_name)
            .await
            .ok_or_else(|| format!("Server not running: {}", server_name))?;

        match self.exchange(&pool, method, arguments.clone()).await {
            Err(CallError::Dead(e)) | Err(CallError::Lost(e)) => {
                eprintln!(
                    "[McpHub][WARN] Connection error on '{}': {}. Retrying...",
                    server_name, e
                );
                self.discard(server_name, &pool).await;
                self.start_server(server_name).await?;
                let pool = self
                    .pool(server_name)
                    .await
                    .ok_or_else(|| format!("Server not running: {}", server_name))?;
                self.exchange(&pool, method, arguments)
                    .await
                    .map_err(CallError::into_message)
            }
            other => other.map_err(CallError::into_message),
        }
    }

    /// Appel d'outil. Jamais rejoué une fois transmis : seule une écriture impossible (rien n'a
    /// été reçu) autorise un redémarrage suivi d'un nouvel essai.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resolved = self
            .resolve_name(server_name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        self.start_server(server_name).await?;
        let pool = self
            .pool(server_name)
            .await
            .ok_or_else(|| format!("Server not running: {}", server_name))?;
        let params = serde_json::json!({ "name": tool_name, "arguments": arguments });

        match self.exchange(&pool, "tools/call", params.clone()).await {
            Ok(result) => Ok(result),
            Err(CallError::Dead(e)) => {
                eprintln!(
                    "[McpHub][WARN] '{}' unreachable before sending ({}): restarting, then one new attempt",
                    server_name, e
                );
                self.discard(server_name, &pool).await;
                self.start_server(server_name).await?;
                // Le serveur relancé a pu changer ses définitions : re-contrôle avant l'envoi.
                if let Some(reason) = crate::toolguard::quarantined(server_name, tool_name) {
                    return Err(format!(
                        "Bloque par McpHub apres redemarrage de {} : outil {} en quarantaine ({})",
                        server_name, tool_name, reason
                    ));
                }
                let pool = self
                    .pool(server_name)
                    .await
                    .ok_or_else(|| format!("Server not running: {}", server_name))?;
                self.exchange(&pool, "tools/call", params)
                    .await
                    .map_err(|e| self.describe_tool_error(server_name, tool_name, e))
            }
            Err(e) => {
                if matches!(e, CallError::Lost(_)) {
                    // Processus perdu : redémarré au prochain appel, l'appel courant n'est pas rejoué.
                    self.discard(server_name, &pool).await;
                }
                Err(self.describe_tool_error(server_name, tool_name, e))
            }
        }
    }

    fn describe_tool_error(&self, server: &str, tool: &str, e: CallError) -> String {
        match e {
            CallError::Lost(detail) | CallError::NoReply(detail) => format!(
                "{} {}/{} : la requete a ete transmise mais aucune reponse n'est arrivee ({}). \
                 L'outil a pu s'executer : verifier l'etat avant de relancer.",
                UNKNOWN_OUTCOME, server, tool, detail
            ),
            CallError::Dead(detail) | CallError::Busy(detail) => format!(
                "{} {}/{} : rien n'a ete execute ({}). Relancer est sans risque.",
                NOT_SENT, server, tool, detail
            ),
            CallError::Server(detail) => detail,
        }
    }

    pub async fn is_running(&self, name: &str) -> bool {
        self.pools.lock().await.contains_key(name)
    }

    pub async fn stop_server(&self, name: &str) {
        let removed = self.pools.lock().await.remove(name);
        if let Some(pool) = removed {
            self.bump_epoch();
            pool.kill_now().await;
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    pub async fn stop_all(&self) {
        let drained: Vec<(String, Arc<ServerPool>)> = self.pools.lock().await.drain().collect();
        self.bump_epoch();
        for (name, pool) in drained {
            pool.kill_now().await;
            eprintln!("[McpHub][INFO] Stopped server: {}", name);
        }
    }

    async fn snapshot(&self) -> Vec<(String, Arc<ServerPool>)> {
        self.pools
            .lock()
            .await
            .iter()
            .map(|(name, pool)| (name.clone(), pool.clone()))
            .collect()
    }

    pub async fn request_all_running(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Vec<(String, Result<serde_json::Value, String>)> {
        let mut results = Vec::new();
        for (name, pool) in self.snapshot().await {
            let res = self
                .exchange(&pool, method, params.clone())
                .await
                .map_err(CallError::into_message);
            results.push((name, res));
        }
        results
    }

    /// Arrête les serveurs inactifs. Un serveur occupé (appel plus long que le délai
    /// d'inactivité) n'est jamais considéré comme inactif.
    pub async fn reap_idle(&self) {
        let timeout = Duration::from_millis(self.idle_timeout_ms);
        for (name, pool) in self.snapshot().await {
            if pool.idle_for() <= timeout || pool.busy() {
                continue;
            }
            if self.remove_if_same(&name, &pool).await {
                pool.kill_now().await;
                eprintln!("[McpHub][INFO] Idle-stopped server: {}", name);
            }
        }
    }

    /// Contrôle de santé sans verrou global : une instance occupée par un appel est vivante,
    /// elle n'est pas pingée (et ne bloque plus les autres serveurs).
    pub async fn health_check(&self) -> Vec<(String, String)> {
        let mut dead_servers: Vec<(String, String)> = Vec::new();

        for (name, pool) in self.snapshot().await {
            let mut reason: Option<String> = None;

            for instance in &pool.procs {
                if instance.busy() {
                    continue;
                }
                // Le ping n'est pas compté comme un appel : il ne doit pas faire passer le
                // serveur pour occupé auprès de l'arrêt sur inactivité.
                let sent = AtomicBool::new(false);
                let ping = Duration::from_secs(5);
                let ping_result = match &instance.link {
                    Link::Stdio(proc) => {
                        let Ok(mut proc) = proc.try_lock() else {
                            continue;
                        };
                        match proc.exited() {
                            Ok(Some(status)) => {
                                reason = Some(format!("Process exited: {}", status));
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                reason = Some(format!("Process check failed: {}", e));
                                break;
                            }
                        }
                        let request = stdio_request(&mut proc, "ping", serde_json::json!({}), &sent);
                        tokio::time::timeout(ping, request).await
                    }
                    Link::Http(_) => {
                        let request = instance.request("ping", serde_json::json!({}), &sent);
                        tokio::time::timeout(ping, request).await
                    }
                };

                match ping_result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        reason = Some(format!("Ping error: {}", e.into_message()));
                        break;
                    }
                    Err(_) => {
                        reason = Some("Ping timeout (5s)".to_string());
                        break;
                    }
                }
            }

            if let Some(reason) = reason {
                if self.remove_if_same(&name, &pool).await {
                    pool.kill_now().await;
                    dead_servers.push((name, reason));
                }
            }
        }

        dead_servers
    }

    pub async fn restart_server(&self, name: &str) -> Result<usize, String> {
        if let Some(pool) = self.pool(name).await {
            self.discard(name, &pool).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tools = self.start_server(name).await?;
        Ok(tools.len())
    }
}

/// Lance une instance, l'initialise et lit ses outils. En cas d'échec, le processus (et ses
/// descendants) est tué avant de rendre l'erreur : aucun orphelin.
async fn start_instance(
    name: &str,
    config: &ServerConfig,
    index: usize,
    pool_size: usize,
    request_timeout_secs: u64,
) -> Result<(Instance, KillHandle, Vec<ToolDef>), String> {
    let start = Instant::now();
    if pool_size > 1 {
        eprintln!(
            "[McpHub][INFO] Starting server: {} (instance {}/{})",
            name,
            index + 1,
            pool_size
        );
    } else {
        eprintln!("[McpHub][INFO] Starting server: {}", name);
    }

    let (link, handle) = if let Some(url) = &config.url {
        if index == 0 {
            eprintln!("[McpHub][INFO] {} : serveur distant {}", name, url);
        }
        let remote = Arc::new(crate::remote::RemoteClient::new(
            name,
            url,
            &config.headers,
            config.oauth,
        ));
        (Link::Http(remote.clone()), KillHandle::Remote(remote))
    } else {
        if index == 0 && config.sandbox.is_some() {
            eprintln!(
                "[McpHub][SANDBOX] {} : isole ({:?})",
                name,
                config.sandbox.as_ref().map(|s| s.mode)
            );
        }
        let mut cmd = local_command(name, config);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(child_stderr(name));
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn {}: {}", name, e))?;
        let pid = child.id().ok_or("No pid")?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill_group(pid);
            return Err("No stdio".into());
        };
        (
            Link::Stdio(Mutex::new(StdioProcess {
                child,
                stdin,
                stdout_lines: BufReader::new(stdout).lines(),
                next_id: 1,
                server_name: name.to_string(),
            })),
            KillHandle::Pid(pid),
        )
    };

    let proc = Instance::new(link);

    match initialize(&proc, name, index, request_timeout_secs).await {
        Ok(tools) => {
            if index == 0 {
                eprintln!(
                    "[McpHub][INFO] Server '{}' ready: {} tools in {:.0}ms",
                    name,
                    tools.len(),
                    start.elapsed().as_secs_f64() * 1000.0
                );
            }
            Ok((proc, handle, tools))
        }
        Err(e) => {
            proc.kill().await;
            Err(e)
        }
    }
}

async fn initialize(
    proc: &Instance,
    name: &str,
    index: usize,
    request_timeout_secs: u64,
) -> Result<Vec<ToolDef>, String> {
    let init_result = send_request(
        proc,
        "initialize",
        serde_json::json!({
            "protocolVersion": crate::protocol::LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "McpHub", "version": env!("CARGO_PKG_VERSION") }
        }),
        request_timeout_secs,
    )
    .await?;

    if let Some(pv) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
        if index == 0 {
            if crate::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&pv) {
                eprintln!("[McpHub][INFO] Server '{}' negotiated protocol: {}", name, pv);
            } else {
                eprintln!(
                    "[McpHub][WARN] Server '{}' answered with protocol {} that McpHub does not implement; tools/list and tools/call are attempted anyway",
                    name, pv
                );
            }
        }
    }

    proc.notify("notifications/initialized", serde_json::json!({})).await?;
    // tools/list est paginé (nextCursor) : on lit toutes les pages.
    let mut tools: Vec<ToolDef> = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..50 {
        let params = match &cursor {
            Some(c) => serde_json::json!({ "cursor": c }),
            None => serde_json::json!({}),
        };
        let tools_result = send_request(proc, "tools/list", params, request_timeout_secs).await?;
        if let Some(arr) = tools_result.get("tools").and_then(|v| v.as_array()) {
            for t in arr {
                // Un outil mal formé n'empêche plus de lire les autres.
                match serde_json::from_value::<ToolDef>(t.clone()) {
                    Ok(def) => tools.push(def),
                    Err(e) => eprintln!(
                        "[McpHub][WARN] {} : outil ignore (definition invalide : {})",
                        name, e
                    ),
                }
            }
        }
        cursor = tools_result
            .get("nextCursor")
            .and_then(|v| v.as_str())
            .filter(|c| !c.is_empty())
            .map(String::from);
        if cursor.is_none() {
            break;
        }
    }

    // Épinglage / détection d'empoisonnement sur la définition réellement annoncée,
    // pour chaque instance du pool.
    crate::toolguard::review(name, &tools);
    Ok(tools)
}

async fn send_request(
    proc: &Instance,
    method: &str,
    params: serde_json::Value,
    timeout_secs: u64,
) -> Result<serde_json::Value, String> {
    let sent = AtomicBool::new(false);
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs.max(1)),
        proc.request(method, params, &sent),
    )
    .await
    {
        Ok(result) => result.map_err(CallError::into_message),
        Err(_) => Err(format!(
            "Timeout: server did not respond within {}s",
            timeout_secs.max(1)
        )),
    }
}

/// Réponse à une requête que le serveur adresse à McpHub (sampling, roots, elicitation) :
/// sans réponse, le serveur attendrait jusqu'à son propre délai.
async fn answer_server_request(
    stdin: &mut tokio::process::ChildStdin,
    id: &serde_json::Value,
    method: &str,
) {
    let reply = if method == "ping" {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("McpHub ne prend pas en charge {}", method) }
        })
    };
    let mut msg = reply.to_string();
    msg.push('\n');
    let _ = stdin.write_all(msg.as_bytes()).await;
    let _ = stdin.flush().await;
}

async fn stdio_request(
    proc: &mut StdioProcess,
    method: &str,
    params: serde_json::Value,
    sent: &AtomicBool,
) -> Result<serde_json::Value, CallError> {
    let id = proc.next_id;
    proc.next_id += 1;
    let server_name = proc.server_name.clone();
    let StdioProcess { stdin, stdout_lines, .. } = proc;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let mut msg = serde_json::to_string(&request).map_err(|e| CallError::Server(e.to_string()))?;
    msg.push('\n');

    // Le saut de ligne final est écrit en dernier : un échec d'écriture signifie qu'aucune
    // requête complète n'a été reçue.
    stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| CallError::Dead(format!("Write error: {}", e)))?;
    sent.store(true, Ordering::SeqCst);
    stdin
        .flush()
        .await
        .map_err(|e| CallError::Lost(format!("Flush error: {}", e)))?;

    loop {
        let line = stdout_lines
            .next_line()
            .await
            .map_err(|e| CallError::Lost(format!("Read error: {}", e)))?
            .ok_or_else(|| CallError::Lost("Server closed connection".into()))?;

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(server_method) = parsed.get("method").and_then(|v| v.as_str()) {
            match parsed.get("id") {
                Some(request_id) => answer_server_request(stdin, request_id, server_method).await,
                None if server_method == "notifications/message" => {
                    if let Some(params) = parsed.get("params") {
                        if let (Some(level), Some(data)) = (
                            params.get("level").and_then(|v| v.as_str()),
                            params.get("data").and_then(|v| v.as_str()),
                        ) {
                            eprintln!(
                                "[McpHub][{}][{}] {}",
                                server_name,
                                level.to_uppercase(),
                                redact_text(data)
                            );
                        }
                    }
                }
                None => {}
            }
            continue;
        }

        if parsed.get("id").and_then(|v| v.as_u64()) == Some(id) {
            if let Some(error) = parsed.get("error") {
                return Err(CallError::Server(format!("MCP error: {}", error)));
            }
            return Ok(parsed
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }
}

async fn stdio_notify(
    proc: &mut StdioProcess,
    method: &str,
    params: serde_json::Value,
) -> Result<(), String> {
    let stdin = &mut proc.stdin;
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });

    let mut msg = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
    msg.push('\n');

    stdin
        .write_all(msg.as_bytes())
        .await
        .map_err(|e| format!("Write error: {}", e))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Faux serveur MCP en sh : chaque démarrage ajoute son pid à `pids`, l'outil `mutate`
    /// écrit dans `mutations` puis ferme sans répondre, `slow` répond après 2 s.
    const FAKE_SERVER: &str = r#"#!/bin/sh
echo $$ >> "$FAKE_DIR/pids"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      if [ -n "$FAKE_HANG_INIT" ]; then sleep 60; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"mutate","description":"m","inputSchema":{"type":"object"}},{"name":"slow","description":"s","inputSchema":{"type":"object"}},{"name":"read_x","description":"r","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"name":"mutate"'*)
      echo done >> "$FAKE_DIR/mutations"
      exit 0 ;;
    *'"name":"slow"'*)
      sleep 2
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"slow"}]}}\n' "$id" ;;
    *'"name":"read_x"'*)
      printf '{"jsonrpc":"2.0","id":99999,"method":"roots/list"}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"x"}]}}\n' "$id" ;;
    *'"method":"ping"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;

    fn fake_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mcphub-fake-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.sh");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        dir
    }

    fn fake_config(dir: &Path, extra_env: &[(&str, &str)], timeout: u64) -> ServerConfig {
        let mut env = HashMap::new();
        env.insert("FAKE_DIR".to_string(), dir.display().to_string());
        for (k, v) in extra_env {
            env.insert(k.to_string(), v.to_string());
        }
        ServerConfig {
            command: "/bin/sh".into(),
            args: vec![dir.join("server.sh").display().to_string()],
            env,
            pool: 1,
            request_timeout_secs: Some(timeout),
            sandbox: None,
            inherit_env: Vec::new(),
            url: None,
            headers: HashMap::new(),
            oauth: false,
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn alive(pid: &str) -> bool {
        let pid: i32 = pid.parse().unwrap();
        // SAFETY: signal 0 ne fait que tester l'existence du processus.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Faux serveur MCP distant (Streamable HTTP, réponses JSON) sur un port local libre.
    /// L'outil `slow` répond après `delay` ; `peak` retient le plus grand nombre d'appels
    /// `slow` traités en même temps.
    async fn fake_http_server(delay: Duration) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let peak = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let peak_seen = peak.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(fake_http_connection(stream, delay, running.clone(), peak_seen.clone()));
            }
        });
        (url, peak)
    }

    async fn fake_http_connection(
        mut stream: tokio::net::TcpStream,
        delay: Duration,
        running: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    ) {
        use tokio::io::AsyncReadExt;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let header_end = loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                let mut chunk = [0u8; 4096];
                match stream.read(&mut chunk).await {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    _ => return,
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            while buf.len() < header_end + length {
                let mut chunk = [0u8; 4096];
                match stream.read(&mut chunk).await {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    _ => return,
                }
            }
            let body: Vec<u8> = buf.drain(..header_end + length).skip(header_end).collect();
            let request: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let id = request.get("id").cloned();
            let result = match request.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": { "name": "fake-http", "version": "1" }
                }),
                Some("tools/list") => serde_json::json!({
                    "tools": [{ "name": "slow", "description": "s", "inputSchema": { "type": "object" } }]
                }),
                Some("tools/call") => {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    serde_json::json!({ "content": [{ "type": "text", "text": "slow" }] })
                }
                _ => serde_json::json!({}),
            };
            let reply = match id {
                Some(id) => {
                    let payload =
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                }
                None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".to_string(),
            };
            if stream.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn remote_config(url: &str, timeout: u64) -> ServerConfig {
        ServerConfig {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            pool: 1,
            request_timeout_secs: Some(timeout),
            sandbox: None,
            inherit_env: Vec::new(),
            url: Some(url.to_string()),
            headers: HashMap::new(),
            oauth: false,
        }
    }

    #[tokio::test]
    async fn appels_distants_simultanes_en_parallele() {
        let delay = Duration::from_millis(600);
        let (url, peak) = fake_http_server(delay).await;
        let mut configs = HashMap::new();
        configs.insert("fake-remote".to_string(), remote_config(&url, 10));
        let manager = ChildManager::new(configs, 300_000, 10);
        manager.start_server("fake-remote").await.unwrap();

        let began = Instant::now();
        let (first, second) = tokio::join!(
            manager.call_tool("fake-remote", "slow", serde_json::json!({})),
            manager.call_tool("fake-remote", "slow", serde_json::json!({})),
        );
        let elapsed = began.elapsed();
        assert_eq!(first.unwrap()["content"][0]["text"], "slow");
        assert_eq!(second.unwrap()["content"][0]["text"], "slow");
        assert_eq!(peak.load(Ordering::SeqCst), 2, "les deux appels doivent se chevaucher");
        assert!(elapsed >= delay, "trop court : {:?}", elapsed);
        assert!(elapsed < delay * 2 - Duration::from_millis(200), "appels en file : {:?}", elapsed);
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn appel_distant_trop_long_issue_inconnue_sans_gener_les_suivants() {
        let (url, _peak) = fake_http_server(Duration::from_millis(1600)).await;
        let mut configs = HashMap::new();
        configs.insert("fake-remote-lent".to_string(), remote_config(&url, 1));
        let manager = ChildManager::new(configs, 300_000, 1);
        manager.start_server("fake-remote-lent").await.unwrap();

        let began = Instant::now();
        let err = manager
            .call_tool("fake-remote-lent", "slow", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        assert!(began.elapsed() < Duration::from_millis(1500), "delai non applique : {:?}", began.elapsed());
        assert!(manager.is_running("fake-remote-lent").await);
        // L'appel abandonné ne compte plus : le serveur redevient arrêtable sur inactivité.
        let pool = manager.pool("fake-remote-lent").await.unwrap();
        assert!(!pool.busy());
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn mutation_perdue_jamais_rejouee() {
        let dir = fake_dir("mutate");
        let mut configs = HashMap::new();
        configs.insert("fake-mutate".to_string(), fake_config(&dir, &[], 5));
        let manager = ChildManager::new(configs, 300_000, 5);

        let err = manager
            .call_tool("fake-mutate", "mutate", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        assert_eq!(lines(&dir.join("mutations")).len(), 1, "la mutation ne doit partir qu'une fois");
        assert!(!manager.is_running("fake-mutate").await, "le processus perdu est retire");

        // Le prochain appel redémarre le serveur normalement.
        let ok = manager
            .call_tool("fake-mutate", "read_x", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(ok["content"][0]["text"], "x");
        assert_eq!(lines(&dir.join("mutations")).len(), 1);
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn serveur_lent_ne_bloque_pas_les_autres() {
        let dir_slow = fake_dir("slow");
        let dir_fast = fake_dir("fast");
        let mut configs = HashMap::new();
        configs.insert("fake-slow".to_string(), fake_config(&dir_slow, &[], 10));
        configs.insert("fake-fast".to_string(), fake_config(&dir_fast, &[], 10));
        let manager = Arc::new(ChildManager::new(configs, 300_000, 10));
        manager.start_server("fake-slow").await.unwrap();
        manager.start_server("fake-fast").await.unwrap();

        let m1 = manager.clone();
        let slow = tokio::spawn(async move {
            m1.call_tool("fake-slow", "slow", serde_json::json!({})).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Contrôle de santé, inactivité, nouveau démarrage et appel au même serveur pendant
        // que le serveur lent travaille : rien ne doit attendre la fin de l'appel lent.
        let m2 = manager.clone();
        let second_slow = tokio::spawn(async move {
            m2.call_tool("fake-slow", "read_x", serde_json::json!({})).await
        });
        let began = Instant::now();
        let dead = manager.health_check().await;
        manager.reap_idle().await;
        manager.start_server("fake-slow").await.unwrap();
        let fast = manager
            .call_tool("fake-fast", "read_x", serde_json::json!({}))
            .await
            .unwrap();
        assert!(dead.is_empty(), "{:?}", dead);
        assert_eq!(fast["content"][0]["text"], "x");
        assert!(began.elapsed() < Duration::from_millis(1000), "bloque {:?}", began.elapsed());

        assert_eq!(slow.await.unwrap().unwrap()["content"][0]["text"], "slow");
        assert_eq!(second_slow.await.unwrap().unwrap()["content"][0]["text"], "x");
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn vingt_premieres_requetes_un_seul_demarrage() {
        let dir = fake_dir("burst");
        let mut configs = HashMap::new();
        configs.insert("fake-burst".to_string(), fake_config(&dir, &[], 10));
        let manager = Arc::new(ChildManager::new(configs, 300_000, 10));

        let calls: Vec<_> = (0..20)
            .map(|_| {
                let m = manager.clone();
                tokio::spawn(async move {
                    m.call_tool("fake-burst", "read_x", serde_json::json!({})).await
                })
            })
            .collect();
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
        assert_eq!(lines(&dir.join("pids")).len(), 1, "un seul processus lance");
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn echec_initialisation_sans_orphelin() {
        let dir = fake_dir("hang");
        let mut configs = HashMap::new();
        configs.insert(
            "fake-hang".to_string(),
            fake_config(&dir, &[("FAKE_HANG_INIT", "1")], 1),
        );
        let manager = ChildManager::new(configs, 300_000, 1);

        assert!(manager.start_server("fake-hang").await.is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pids = lines(&dir.join("pids"));
        assert_eq!(pids.len(), 3, "trois tentatives");
        for pid in &pids {
            assert!(!alive(pid), "processus {} toujours vivant", pid);
        }
        // Les appels suivants réutilisent l'erreur récente au lieu de relancer trois essais.
        assert!(manager.start_server("fake-hang").await.is_err());
        assert_eq!(lines(&dir.join("pids")).len(), 3);
    }
}
