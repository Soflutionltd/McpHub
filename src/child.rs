/// Child process manager: spawn MCP servers, communicate over stdio, manage lifecycle.
///
/// Verrous : la table globale des pools n'est jamais tenue pendant une attente (appel, ping).
/// Aucun verrou n'est tenu pendant l'attente d'une réponse : un serveur distant reçoit ses
/// requêtes HTTP en parallèle, un processus local multiplexe les siennes par identifiant
/// JSON-RPC (une tâche lectrice par processus, écritures sur stdin sérialisées par un verrou
/// tenu le temps d'une ligne).
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

pub use crate::config::ServerConfig;
use crate::protocol::ToolDef;
use crate::redaction::redact_text;

/// Préfixe d'une erreur où la requête n'a jamais atteint le serveur : la relancer est sans risque.
pub const NOT_SENT: &str = "[non transmis]";

/// Clé `_meta` d'un `tools/call` qui porte le dossier de la session à l'origine de l'appel, au format
/// d'une réponse `roots/list` : un serveur partagé par plusieurs sessions sait ainsi, appel par appel
/// et sans aller-retour, dans quel projet il travaille.
pub const ROOTS_META: &str = "com.soflution.mcphub/roots";

tokio::task_local! {
    /// Dossier de la session dont vient l'appel en cours (`working_dir`, sinon le dossier du client).
    pub static CALL_ROOT: Option<std::path::PathBuf>;
}

/// Le dossier de la session à l'origine de l'appel en cours, s'il est connu.
pub fn call_root() -> Option<std::path::PathBuf> {
    CALL_ROOT.try_with(|root| root.clone()).ok().flatten()
}

/// URI `file://` d'un dossier absolu, chaque octet hors des caractères non réservés encodé en %XX.
fn file_uri(path: &std::path::Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{:02X}", byte));
        }
    }
    uri
}

/// Les racines d'une session, au format MCP de `roots/list` : le dossier, nommé par son dernier élément.
pub fn roots_value(root: Option<&std::path::Path>) -> serde_json::Value {
    let roots: Vec<serde_json::Value> = root
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            serde_json::json!({ "uri": file_uri(path), "name": name })
        })
        .into_iter()
        .collect();
    serde_json::Value::Array(roots)
}
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

type Reply = Result<serde_json::Value, CallError>;

/// Où en est l'envoi d'une requête. L'appelant qui renonce (délai dépassé) et la tâche qui
/// écrit se départagent par un seul échange atomique : soit la requête est abandonnée avant
/// le premier octet et ne sera jamais écrite, soit elle est considérée comme transmise.
#[derive(Clone)]
struct SendState(Arc<AtomicU8>);

impl SendState {
    const QUEUED: u8 = 0;
    const WRITING: u8 = 1;
    const SENT: u8 = 2;
    const ABANDONED: u8 = 3;

    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(Self::QUEUED)))
    }

    /// Côté écriture : faux si l'appelant a déjà renoncé, rien ne doit alors être écrit.
    fn begin_write(&self) -> bool {
        self.0
            .compare_exchange(
                Self::QUEUED,
                Self::WRITING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    fn mark_sent(&self) {
        self.0.store(Self::SENT, Ordering::SeqCst);
    }

    /// Côté appelant : vrai si la requête est prouvée non envoyée (et ne le sera jamais).
    /// Une écriture commencée compte comme transmise : l'issue est alors inconnue.
    fn abandon(&self) -> bool {
        match self.0.compare_exchange(
            Self::QUEUED,
            Self::ABANDONED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(actual) => actual == Self::ABANDONED,
        }
    }
}

/// Renonce à l'envoi si le futur de l'appel est abandonné avant l'écriture.
struct AbandonOnDrop<'a>(&'a SendState);

impl Drop for AbandonOnDrop<'_> {
    fn drop(&mut self) {
        self.0.abandon();
    }
}

#[derive(Default)]
struct Waiting {
    replies: HashMap<u64, oneshot::Sender<Reply>>,
    /// Raison de l'arrêt de la lecture (processus mort, stdout fermé) : plus rien n'arrivera.
    closed: Option<String>,
}

/// Partie d'un processus local partagée entre les appelants, la tâche lectrice et les
/// tâches d'écriture.
struct StdioShared {
    server_name: String,
    stdin: Mutex<tokio::process::ChildStdin>,
    waiting: std::sync::Mutex<Waiting>,
    next_id: AtomicU64,
    /// Dossier de la session du dernier appel transmis : ce que `roots/list` répond.
    root: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl StdioShared {
    fn waiting(&self) -> std::sync::MutexGuard<'_, Waiting> {
        // Aucune section critique ne peut paniquer en laissant la table incohérente.
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, id: u64, reply: oneshot::Sender<Reply>) -> Result<(), CallError> {
        let mut waiting = self.waiting();
        if let Some(reason) = &waiting.closed {
            return Err(CallError::Dead(reason.clone()));
        }
        waiting.replies.insert(id, reply);
        Ok(())
    }

    fn forget(&self, id: u64) {
        self.waiting().replies.remove(&id);
    }

    /// Fin de la lecture : tous les appels en attente sont libérés. Ceux déjà écrits y
    /// lisent une issue inconnue ; ceux pas encore écrits échoueront à l'écriture (non
    /// transmis) sans regarder cette réponse.
    fn close(&self, reason: String) {
        let pending = {
            let mut waiting = self.waiting();
            waiting.closed = Some(reason.clone());
            std::mem::take(&mut waiting.replies)
        };
        for (_, reply) in pending {
            let _ = reply.send(Err(CallError::Lost(reason.clone())));
        }
    }

    /// Route une ligne de stdout : réponse vers l'appel qui l'attend, requête du serveur,
    /// notification. Une réponse sans appel en attente (délai déjà dépassé) est ignorée.
    fn dispatch(self: &Arc<Self>, line: &str) {
        if line.is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };

        if let Some(server_method) = parsed.get("method").and_then(|v| v.as_str()) {
            match parsed.get("id") {
                // Réponse écrite hors de la tâche lectrice : stdin plein ne doit jamais
                // empêcher de lire stdout.
                Some(request_id) => {
                    tokio::spawn(answer_server_request(
                        self.clone(),
                        request_id.clone(),
                        server_method.to_string(),
                    ));
                }
                None if server_method == "notifications/tools/list_changed" => {
                    notify_tools_changed(&self.server_name);
                }
                None if server_method == "notifications/message" => {
                    if let Some(params) = parsed.get("params") {
                        if let (Some(level), Some(data)) = (
                            params.get("level").and_then(|v| v.as_str()),
                            params.get("data").and_then(|v| v.as_str()),
                        ) {
                            eprintln!(
                                "[McpHub][{}][{}] {}",
                                self.server_name,
                                level.to_uppercase(),
                                redact_text(data)
                            );
                        }
                    }
                }
                None => {}
            }
            return;
        }

        let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) else {
            return;
        };
        let Some(reply) = self.waiting().replies.remove(&id) else {
            return;
        };
        let outcome = match parsed.get("error") {
            Some(error) => Err(CallError::Server(format!("MCP error: {}", error))),
            None => Ok(parsed
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null)),
        };
        // L'appelant a pu renoncer entre-temps : sa réponse tardive est simplement perdue.
        let _ = reply.send(outcome);
    }
}

/// Retire l'appel de la table quand il se termine ou que son futur est abandonné.
struct Registered<'a> {
    shared: &'a StdioShared,
    id: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.shared.forget(self.id);
    }
}

/// Tâche lectrice : une par processus, jusqu'à la fermeture de stdout.
async fn read_replies(
    shared: Arc<StdioShared>,
    mut stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) {
    let reason = loop {
        match stdout_lines.next_line().await {
            Ok(Some(line)) => shared.dispatch(line.trim()),
            Ok(None) => break "Server closed connection".to_string(),
            Err(e) => break format!("Read error: {}", e),
        }
    };
    shared.close(reason);
}

/// Écrit une requête entière sur stdin. Lancée dans sa propre tâche : un appelant qui
/// renonce en cours d'écriture ne laisse pas une ligne tronquée devant la requête suivante.
async fn write_request(
    shared: Arc<StdioShared>,
    line: Vec<u8>,
    state: SendState,
) -> Result<(), CallError> {
    let mut stdin = shared.stdin.lock().await;
    if !state.begin_write() {
        return Err(CallError::Busy("request abandoned before sending".into()));
    }
    // Le saut de ligne final est écrit en dernier : un échec d'écriture signifie qu'aucune
    // requête complète n'a été reçue.
    stdin
        .write_all(&line)
        .await
        .map_err(|e| CallError::Dead(format!("Write error: {}", e)))?;
    state.mark_sent();
    stdin
        .flush()
        .await
        .map_err(|e| CallError::Lost(format!("Flush error: {}", e)))
}

struct StdioProcess {
    shared: Arc<StdioShared>,
    child: Mutex<Child>,
}

impl StdioProcess {
    fn start(
        server_name: &str,
        child: Child,
        stdin: tokio::process::ChildStdin,
        stdout: tokio::process::ChildStdout,
    ) -> Self {
        let shared = Arc::new(StdioShared {
            server_name: server_name.to_string(),
            stdin: Mutex::new(stdin),
            waiting: std::sync::Mutex::new(Waiting::default()),
            next_id: AtomicU64::new(1),
            root: std::sync::Mutex::new(None),
        });
        tokio::spawn(read_replies(shared.clone(), BufReader::new(stdout).lines()));
        Self {
            shared,
            child: Mutex::new(child),
        }
    }

    async fn kill(&self) {
        let mut child = self.child.lock().await;
        if let Some(pid) = child.id() {
            kill_group(pid);
        }
        let _ = child.kill().await;
    }

    async fn exited(&self) -> Result<Option<String>, String> {
        match self.child.lock().await.try_wait() {
            Ok(Some(status)) => Ok(Some(status.to_string())),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Un échange : l'appel s'inscrit dans la table avant d'écrire (la réponse ne peut pas
    /// arriver avant), puis attend sa réponse sans tenir aucun verrou.
    async fn request(&self, method: &str, params: serde_json::Value, state: &SendState) -> Reply {
        let shared = &self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line =
            serde_json::to_vec(&request).map_err(|e| CallError::Server(e.to_string()))?;
        line.push(b'\n');

        let (reply, answered) = oneshot::channel();
        shared.register(id, reply)?;
        let _registered = Registered { shared, id };
        let _abandon = AbandonOnDrop(state);

        match tokio::spawn(write_request(shared.clone(), line, state.clone())).await {
            Ok(written) => written?,
            Err(e) if state.abandon() => {
                return Err(CallError::Dead(format!("Write task stopped: {}", e)))
            }
            Err(e) => return Err(CallError::Lost(format!("Write task stopped: {}", e))),
        }

        answered
            .await
            .unwrap_or_else(|_| Err(CallError::Lost("Server closed connection".into())))
    }

    /// Note le dossier de la session de l'appel qui part ; s'il change, le serveur en est averti
    /// (`notifications/roots/list_changed`) avant de recevoir l'appel.
    async fn note_root(&self, root: &std::path::Path) {
        let changed = {
            let mut current = self
                .shared
                .root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = current.as_deref() != Some(root);
            *current = Some(root.to_path_buf());
            changed
        };
        if changed {
            // Au mieux : un serveur qui ne lit plus recevra quand même l'appel et son `_meta`.
            let _ = self
                .notify("notifications/roots/list_changed", serde_json::json!({}))
                .await;
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&notification).map_err(|e| e.to_string())?;
        line.push(b'\n');
        tokio::spawn(write_request(self.shared.clone(), line, SendState::new()))
            .await
            .map_err(|e| format!("Write task stopped: {}", e))?
            .map_err(|e| match e {
                CallError::Dead(e)
                | CallError::Busy(e)
                | CallError::Lost(e)
                | CallError::NoReply(e)
                | CallError::Server(e) => e,
            })
    }
}

enum Link {
    Stdio(Box<StdioProcess>),
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
        Self {
            link,
            in_flight: AtomicUsize::new(0),
        }
    }

    fn busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    async fn kill(&self) {
        match &self.link {
            Link::Stdio(proc) => proc.kill().await,
            Link::Http(remote) => remote.close().await,
        }
    }

    /// Un échange JSON-RPC, sans verrou tenu pendant l'attente de la réponse.
    /// Note le dossier de la session de l'appel qui part (serveurs locaux : seuls à demander `roots/list`).
    async fn note_root(&self, root: &std::path::Path) {
        if let Link::Stdio(proc) = &self.link {
            proc.note_root(root).await;
        }
    }

    async fn request(&self, method: &str, params: serde_json::Value, state: &SendState) -> Reply {
        match &self.link {
            Link::Http(remote) => {
                state.mark_sent();
                remote
                    .request(method, params)
                    .await
                    .map_err(CallError::Server)
            }
            Link::Stdio(proc) => proc.request(method, params, state).await,
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        match &self.link {
            Link::Http(remote) => remote.notify(method, params).await,
            Link::Stdio(proc) => proc.notify(method, params).await,
        }
    }
}

/// De quoi arrêter un processus sans attendre quoi que ce soit de lui.
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
    /// Outils annoncés, relus quand le serveur signale un changement (`tools/list_changed`).
    tools: std::sync::Mutex<Vec<ToolDef>>,
    request_timeout_secs: u64,
    next_idx: AtomicUsize,
    last_used_ms: AtomicU64,
}

impl ServerPool {
    fn tools(&self) -> Vec<ToolDef> {
        self.tools.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)))
    }

    /// L'instance la moins chargée (`pool` > 1), à tour de rôle en cas d'égalité. Chaque
    /// instance accepte plusieurs appels en vol : `pool` répartit la charge, il ne la borne pas.
    fn pick(&self) -> Arc<Instance> {
        let count = self.procs.len();
        let first = self.next_idx.fetch_add(1, Ordering::Relaxed) % count;
        let mut best = &self.procs[first];
        for offset in 1..count {
            let candidate = &self.procs[(first + offset) % count];
            if candidate.in_flight.load(Ordering::SeqCst) < best.in_flight.load(Ordering::SeqCst) {
                best = candidate;
            }
        }
        best.clone()
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

/// Serveurs qui ont annoncé un changement de leurs outils, lus par le proxy (catalogue).
fn tools_changed_channel() -> &'static std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>> {
    static C: std::sync::OnceLock<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(None))
}

/// Le proxy s'abonne aux changements d'outils annoncés par les serveurs.
pub fn subscribe_tools_changed() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *tools_changed_channel().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    rx
}

fn notify_tools_changed(server: &str) {
    let guard = tools_changed_channel().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(server.to_string());
    }
}

/// Variables système transmises à tous les serveurs.
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "__CF_USER_TEXT_ENCODING",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "TZ",
    "NVM_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
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
pub fn local_command(name: &str, config: &ServerConfig) -> Result<Command, String> {
    let (program, program_args) = crate::sandbox::wrap(
        name,
        config.sandbox.as_ref(),
        &config.command,
        &config.args,
        &config.env,
    )?;
    let mut cmd = Command::new(&program);
    cmd.args(&program_args).kill_on_drop(true);
    cmd.env_clear();
    // Instance d'un tenant : jamais l'agent SSH ni les variables héritées du démon.
    let inherit: &[String] = if config.tenant.is_some() {
        &[]
    } else {
        &config.inherit_env
    };
    for (k, v) in base_child_env(config.sandbox.is_none() && config.tenant.is_none(), inherit) {
        cmd.env(k, v);
    }
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    Ok(cmd)
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{}.stderr.log", safe));
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false);
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

/// Verrou de démarrage d'un serveur : dernier échec (instant, message) s'il y en a un.
type StartLock = Arc<Mutex<Option<(Instant, String)>>>;

pub struct ChildManager {
    configs: Arc<Mutex<HashMap<String, ServerConfig>>>,
    pools: Arc<Mutex<HashMap<String, Arc<ServerPool>>>>,
    /// Un verrou de démarrage par serveur : vingt premières requêtes = un seul démarrage.
    starting: Mutex<HashMap<String, StartLock>>,
    idle_timeout_ms: u64,
    request_timeout_secs: u64,
    /// Augmente à chaque serveur perdu, relancé ou arrêté : les réponses mises en cache avant
    /// ne sont plus servies (l'état du serveur a pu changer).
    epoch: AtomicU64,
    /// Serveurs préchauffés, épargnés par l'arrêt d'inactivité (voir `set_keep_warm`).
    keep_warm: std::sync::RwLock<HashSet<String>>,
}

/// Erreur de démarrage qui ne disparaît pas en réessayant : authentification refusée ou à
/// refaire, programme introuvable ou non exécutable. Tout le reste (connexion coupée, délai
/// dépassé, serveur occupé) peut passer au second essai et reste réessayé.
pub fn is_permanent_start_error(error: &str) -> bool {
    let e = error.to_lowercase();
    // « jeton refuse (503 ...) » ou (429) est un incident du serveur d'autorisation, passager :
    // seuls les refus du jeton lui-même (400, 401, invalid_grant) sont définitifs.
    const AUTH: [&str; 8] = [
        "authentification refusee (401)",
        "jeton refuse (400",
        "jeton refuse (401",
        "invalid_grant",
        "invalid_client",
        "connexion requise",
        "not authenticated",
        "api key is missing",
    ];
    AUTH.iter().any(|needle| e.contains(needle))
        // Avec la parenthèse : « os error 2 » est aussi le début de 23, 24 (trop de fichiers
        // ouverts) et 28 (disque plein), qui passent d'eux-mêmes.
        || (e.contains("failed to spawn")
            && (e.contains("(os error 2)") || e.contains("(os error 13)")))
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
            keep_warm: std::sync::RwLock::new(HashSet::new()),
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

    /// Installe ou remplace la config d'une instance de tenant. Une instance dont la config
    /// change (secret modifié, config de base modifiée) est arrêtée et relancée au prochain appel.
    pub async fn set_instance_config(&self, name: &str, config: ServerConfig) {
        let changed = {
            let mut configs = self.configs.lock().await;
            let changed = configs.get(name).is_some_and(|old| *old != config);
            configs.insert(name.to_string(), config);
            changed
        };
        if changed {
            self.stop_server(name).await;
        }
    }

    /// Retire toutes les instances d'un tenant (configs et processus).
    pub async fn remove_tenant_instances(&self, tenant: &str) {
        let of_tenant =
            |name: &String| crate::tenant::split_instance(name).is_some_and(|(_, t)| t == tenant);
        let mut names: Vec<String> = self
            .configs
            .lock()
            .await
            .keys()
            .filter(|n| of_tenant(n))
            .cloned()
            .collect();
        names.extend(
            self.pools
                .lock()
                .await
                .keys()
                .filter(|n| of_tenant(n))
                .cloned(),
        );
        names.sort();
        names.dedup();
        for name in names {
            self.remove_instance(&name).await;
        }
    }

    /// Retire une instance de tenant (config et processus).
    pub async fn remove_instance(&self, name: &str) {
        if !crate::tenant::is_instance_name(name) {
            return;
        }
        self.configs.lock().await.remove(name);
        self.stop_server(name).await;
    }

    pub async fn has_config(&self, name: &str) -> bool {
        self.configs.lock().await.contains_key(name)
    }

    /// Nom exact d'une instance de tenant, sinon résolution normale.
    async fn resolve_internal(&self, name: &str) -> Option<String> {
        if crate::tenant::is_instance_name(name) {
            return self
                .configs
                .lock()
                .await
                .contains_key(name)
                .then(|| name.to_string());
        }
        self.resolve_name(name).await
    }

    /// Nom canonique (clé de config) d'un serveur. Utilisé aussi par le proxy pour que
    /// les permissions s'appliquent exactement au serveur qui sera appelé.
    ///
    /// Un nom venu de l'extérieur ne désigne jamais une instance de tenant (`serveur@tenant`) :
    /// celles-ci ne s'atteignent que par le routage des tenants (`resolve_instance`).
    pub async fn resolve_name(&self, name: &str) -> Option<String> {
        if crate::tenant::is_instance_name(name) {
            return None;
        }
        let configs = self.configs.lock().await;
        if configs.contains_key(name) {
            return Some(name.to_string());
        }
        let lower = name.to_lowercase();
        for key in configs
            .keys()
            .filter(|k| !crate::tenant::is_instance_name(k))
        {
            if key.to_lowercase() == lower {
                return Some(key.clone());
            }
        }
        let normalized = lower.replace(['-', '_'], "");
        for key in configs
            .keys()
            .filter(|k| !crate::tenant::is_instance_name(k))
        {
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

    /// Relit les outils d'un serveur en marche (après `notifications/tools/list_changed`) :
    /// définitions contrôlées et épinglées comme au démarrage. `None` si le serveur est arrêté.
    pub async fn refresh_tools(&self, name: &str) -> Option<Result<Vec<ToolDef>, String>> {
        let pool = self.pool(name).await?;
        let proc = pool.pick();
        let result = list_tools(&proc, name, pool.request_timeout_secs).await;
        if let Ok(tools) = &result {
            *pool.tools.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = tools.clone();
        }
        Some(result)
    }

    pub async fn start_server(&self, name: &str) -> Result<Vec<ToolDef>, String> {
        let name_resolved = self
            .resolve_internal(name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", name))?;
        let name = name_resolved.as_str();

        if let Some(pool) = self.pool(name).await {
            pool.touch();
            return Ok(pool.tools());
        }

        let gate = {
            let mut starting = self.starting.lock().await;
            starting.entry(name.to_string()).or_default().clone()
        };
        let mut last_failure = gate.lock().await;
        if let Some(pool) = self.pool(name).await {
            pool.touch();
            return Ok(pool.tools());
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
                    // Un refus d'authentification ou un programme introuvable ne se repare pas en
                    // reessayant : l'agent a la reponse tout de suite au lieu d'attendre 3,5 s.
                    if is_permanent_start_error(&last_error) {
                        eprintln!(
                            "[McpHub][WARN] {} failed: {} (not retried: needs a fix, not time)",
                            name, last_error
                        );
                        let error = format!("{} (not retried)", last_error);
                        *last_failure = Some((Instant::now(), error.clone()));
                        return Err(error);
                    }
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
            tools: std::sync::Mutex::new(first_tools.clone()),
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
    /// l'attente de l'écriture (stdin d'un processus local qui ne lit plus).
    async fn exchange(
        &self,
        pool: &ServerPool,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        pool.touch();
        let instance = pool.pick();
        let _in_flight = InFlight::enter(&instance.in_flight);
        if method == "tools/call" {
            if let Some(root) = call_root() {
                instance.note_root(&root).await;
            }
        }
        let secs = pool.request_timeout_secs;
        let state = SendState::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(secs),
            instance.request(method, params, &state),
        )
        .await;
        pool.touch();
        match outcome {
            Ok(result) => result,
            Err(_) if state.abandon() => Err(CallError::Busy(format!(
                "Timeout: server busy for {}s, request not sent",
                secs
            ))),
            Err(_) => Err(CallError::NoReply(format!(
                "Timeout: server did not respond within {}s",
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
            .resolve_internal(server_name)
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
            .resolve_internal(server_name)
            .await
            .ok_or_else(|| format!("Unknown server: {}", server_name))?;
        let server_name = resolved.as_str();

        self.start_server(server_name).await?;
        let pool = self
            .pool(server_name)
            .await
            .ok_or_else(|| format!("Server not running: {}", server_name))?;
        let mut params = serde_json::json!({ "name": tool_name, "arguments": arguments });
        if let Some(root) = call_root() {
            params["_meta"] = serde_json::json!({ ROOTS_META: roots_value(Some(&root)) });
        }

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

    /// Serveurs à ne jamais arrêter pour inactivité : ceux que l'utilisateur a demandé de
    /// préchauffer. Les arrêter après une pause les ferait redémarrer à froid à l'appel suivant.
    pub fn set_keep_warm(&self, names: impl IntoIterator<Item = String>) {
        *self.keep_warm.write().unwrap_or_else(|e| e.into_inner()) = names.into_iter().collect();
    }

    /// Après un rechargement de configuration : oublie les serveurs qui n'existent plus.
    pub fn retain_keep_warm(&self, keep: impl Fn(&str) -> bool) {
        self.keep_warm
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|name| keep(name));
    }

    fn kept_warm(&self, name: &str) -> bool {
        self.keep_warm
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }

    /// Arrête les serveurs inactifs. Un serveur occupé (appel plus long que le délai
    /// d'inactivité) n'est jamais considéré comme inactif, un serveur gardé au chaud non plus.
    pub async fn reap_idle(&self) {
        let timeout = Duration::from_millis(self.idle_timeout_ms);
        for (name, pool) in self.snapshot().await {
            if pool.idle_for() <= timeout || pool.busy() || self.kept_warm(&name) {
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
                if let Link::Stdio(proc) = &instance.link {
                    match proc.exited().await {
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
                }

                // Le ping n'est pas compté comme un appel : il ne doit pas faire passer le
                // serveur pour occupé auprès de l'arrêt sur inactivité.
                let state = SendState::new();
                let ping_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    instance.request("ping", serde_json::json!({}), &state),
                )
                .await;

                match ping_result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        reason = Some(format!("Ping error: {}", e.into_message()));
                        break;
                    }
                    // Un appel arrivé pendant le ping occupe le serveur : il est vivant.
                    Err(_) if instance.busy() => {}
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
        let mut cmd = local_command(name, config)?;
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
            Link::Stdio(Box::new(StdioProcess::start(name, child, stdin, stdout))),
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
            // Les racines : le dossier de la session de chaque appel (`roots/list`, `_meta`).
            "capabilities": { "roots": { "listChanged": true } },
            "clientInfo": { "name": "McpHub", "version": env!("CARGO_PKG_VERSION") }
        }),
        request_timeout_secs,
    )
    .await?;

    if let Some(pv) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
        if index == 0 {
            if crate::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&pv) {
                eprintln!(
                    "[McpHub][INFO] Server '{}' negotiated protocol: {}",
                    name, pv
                );
            } else {
                eprintln!(
                    "[McpHub][WARN] Server '{}' answered with protocol {} that McpHub does not implement; tools/list and tools/call are attempted anyway",
                    name, pv
                );
            }
        }
    }

    proc.notify("notifications/initialized", serde_json::json!({}))
        .await?;
    list_tools(proc, name, request_timeout_secs).await
}

/// Lit `tools/list` (paginé, nextCursor) puis épingle et contrôle les définitions annoncées.
async fn list_tools(proc: &Instance, name: &str, request_timeout_secs: u64) -> Result<Vec<ToolDef>, String> {
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
    let state = SendState::new();
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs.max(1)),
        proc.request(method, params, &state),
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

/// Réponse à une requête que le serveur adresse à McpHub : `ping`, `roots/list` (le dossier de la
/// session du dernier appel) ; sampling et elicitation refusés. Sans réponse, le serveur attendrait
/// jusqu'à son propre délai.
async fn answer_server_request(shared: Arc<StdioShared>, id: serde_json::Value, method: String) {
    let reply = if method == "ping" {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else if method == "roots/list" {
        let root = shared
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": roots_value(root.as_deref()) } })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("McpHub ne prend pas en charge {}", method) }
        })
    };
    let mut line = reply.to_string().into_bytes();
    line.push(b'\n');
    // Au mieux, comme avant : un serveur qui ne lit plus n'aura pas sa réponse.
    let _ = write_request(shared, line, SendState::new()).await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Faux serveur MCP en sh : chaque démarrage ajoute son pid à `pids`, l'outil `mutate`
    /// écrit dans `mutations` puis ferme sans répondre, `slow` répond après 2 s en bloquant
    /// la lecture. `wait` (1 s) et `late` (3 s) répondent en tâche de fond, la lecture
    /// continue ; `hang` note l'appel dans `received` et ne répond jamais ; `stall` fait de
    /// même puis cesse de lire stdin. Une réponse de McpHub à une requête du serveur (-32601, ou
    /// les racines de `roots/list`) va dans `answers` ; `where` note l'appel reçu dans `calls`
    /// puis demande `roots/list` ; un changement de racines annoncé va dans `changed`.
    const FAKE_SERVER: &str = r#"#!/bin/sh
echo $$ >> "$FAKE_DIR/pids"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      if [ -n "$FAKE_HANG_INIT" ]; then sleep 60; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      extra=''
      if [ -f "$FAKE_DIR/grown" ]; then extra=',{"name":"extra","description":"e","inputSchema":{"type":"object"}}'; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"mutate","description":"m","inputSchema":{"type":"object"}},{"name":"slow","description":"s","inputSchema":{"type":"object"}},{"name":"read_x","description":"r","inputSchema":{"type":"object"}}%s]}}\n' "$id" "$extra" ;;
    *'"name":"grow"'*)
      touch "$FAKE_DIR/grown"
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"grown"}]}}\n' "$id" ;;
    *'"name":"mutate"'*)
      echo done >> "$FAKE_DIR/mutations"
      exit 0 ;;
    *'"name":"slow"'*)
      sleep 2
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"slow"}]}}\n' "$id" ;;
    *'"name":"wait"'*)
      ( sleep 1; printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"wait"}]}}\n' "$id" ) & ;;
    *'"name":"late"'*)
      ( sleep 3; printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"late"}]}}\n' "$id" ) & ;;
    *'"name":"hang"'*)
      echo "hang $id" >> "$FAKE_DIR/received" ;;
    *'"name":"stall"'*)
      echo "stall $id" >> "$FAKE_DIR/received"
      sleep 30 ;;
    *'-32601'*|*'"result":{"roots":'*)
      echo "$line" >> "$FAKE_DIR/answers" ;;
    *'roots/list_changed'*)
      echo changed >> "$FAKE_DIR/changed" ;;
    *'"name":"where"'*)
      echo "$line" >> "$FAKE_DIR/calls"
      printf '{"jsonrpc":"2.0","id":99998,"method":"roots/list"}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"here"}]}}\n' "$id" ;;
    *'"name":"read_x"'*)
      echo "read_x $id" >> "$FAKE_DIR/received"
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
            tenant: None,
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
                tokio::spawn(fake_http_connection(
                    stream,
                    delay,
                    running.clone(),
                    peak_seen.clone(),
                ));
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
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                            .to_string();
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
            tenant: None,
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
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "les deux appels doivent se chevaucher"
        );
        assert!(elapsed >= delay, "trop court : {:?}", elapsed);
        assert!(
            elapsed < delay * 2 - Duration::from_millis(200),
            "appels en file : {:?}",
            elapsed
        );
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
        assert!(
            began.elapsed() < Duration::from_millis(1500),
            "delai non applique : {:?}",
            began.elapsed()
        );
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
        assert_eq!(
            lines(&dir.join("mutations")).len(),
            1,
            "la mutation ne doit partir qu'une fois"
        );
        assert!(
            !manager.is_running("fake-mutate").await,
            "le processus perdu est retire"
        );

        // Le prochain appel redémarre le serveur normalement.
        let ok = manager
            .call_tool("fake-mutate", "read_x", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(ok["content"][0]["text"], "x");
        assert_eq!(lines(&dir.join("mutations")).len(), 1);
        manager.stop_all().await;
    }

    #[test]
    fn erreurs_definitives_reconnues() {
        for e in [
            "aqara : authentification refusee (401). Verifier le jeton dans headers.",
            r#"jeton refuse (400 Bad Request) : {"error":"invalid_grant","error_description":"refresh token expired"}"#,
            "docix : connexion requise. Lancer : McpHub oauth login docix",
            r#"MCP error: {"code":-32001,"message":"Not authenticated - your API key is missing"}"#,
            "Failed to spawn desktop-commander: No such file or directory (os error 2)",
            "Failed to spawn outil: Permission denied (os error 13)",
        ] {
            assert!(is_permanent_start_error(e), "{}", e);
        }
        for e in [
            "Server closed connection",
            "Timeout: server did not respond within 30s",
            "Failed to spawn outil: Resource temporarily unavailable (os error 35)",
            "Failed to spawn outil: Too many open files (os error 24)",
            "Failed to spawn outil: No space left on device (os error 28)",
            "jeton refuse (503 Service Unavailable) : upstream",
            "jeton refuse (429 Too Many Requests) : slow down",
            r#"MCP error: {"code":-32000,"message":"upstream Unauthorized, retry later"}"#,
            "HTTP 403 Forbidden",
            "HTTP 401 on a proxy in front (401) of the server",
            "HTTP 503 Service Unavailable",
            "connection reset by peer",
        ] {
            assert!(!is_permanent_start_error(e), "{}", e);
        }
    }

    #[tokio::test]
    async fn programme_introuvable_pas_reessaye() {
        let dir = fake_dir("absent");
        let mut config = fake_config(&dir, &[], 10);
        config.command = "/nonexistent/mcphub-test-binary".into();
        let mut configs = HashMap::new();
        configs.insert("absent".to_string(), config);
        let manager = ChildManager::new(configs, 300_000, 10);
        let began = Instant::now();
        let error = manager.start_server("absent").await.unwrap_err();
        // Trois essais auraient attendu 500 + 1000 + 2000 ms.
        assert!(
            began.elapsed() < Duration::from_millis(400),
            "{:?}",
            began.elapsed()
        );
        assert!(error.contains("not retried"), "{}", error);
    }

    #[tokio::test]
    async fn serveur_garde_au_chaud_pas_arrete() {
        let dir_warm = fake_dir("warm");
        let dir_cold = fake_dir("cold");
        let mut configs = HashMap::new();
        configs.insert("warm".to_string(), fake_config(&dir_warm, &[], 10));
        configs.insert("cold".to_string(), fake_config(&dir_cold, &[], 10));
        // Délai d'inactivité de 1 ms : tout serveur au repos est arrêtable.
        let manager = ChildManager::new(configs, 1, 10);
        manager.set_keep_warm(["warm".to_string()]);
        manager.start_server("warm").await.unwrap();
        manager.start_server("cold").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        manager.reap_idle().await;
        assert!(
            manager.pool("warm").await.is_some(),
            "le serveur garde au chaud a ete arrete"
        );
        assert!(
            manager.pool("cold").await.is_none(),
            "le serveur ordinaire devait etre arrete"
        );
        // Retiré de la liste, il redevient arrêtable.
        manager.set_keep_warm(Vec::<String>::new());
        manager.reap_idle().await;
        assert!(manager.pool("warm").await.is_none());
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
            m1.call_tool("fake-slow", "slow", serde_json::json!({}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Contrôle de santé, inactivité, nouveau démarrage et appel au même serveur pendant
        // que le serveur lent travaille : rien ne doit attendre la fin de l'appel lent.
        let m2 = manager.clone();
        let second_slow = tokio::spawn(async move {
            m2.call_tool("fake-slow", "read_x", serde_json::json!({}))
                .await
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
        assert!(
            began.elapsed() < Duration::from_millis(1000),
            "bloque {:?}",
            began.elapsed()
        );

        assert_eq!(slow.await.unwrap().unwrap()["content"][0]["text"], "slow");
        assert_eq!(
            second_slow.await.unwrap().unwrap()["content"][0]["text"],
            "x"
        );
        manager.stop_all().await;
    }

    /// Attend qu'un fichier du faux serveur contienne `count` lignes (5 s au plus).
    async fn wait_lines(path: &Path, count: usize) -> Vec<String> {
        for _ in 0..100 {
            let found = lines(path);
            if found.len() >= count {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "{} : {} ligne(s) attendue(s), trouve {:?}",
            path.display(),
            count,
            lines(path)
        );
    }

    fn kill_fake(dir: &Path) {
        let pids = lines(&dir.join("pids"));
        kill_group(pids.last().expect("un processus lance").parse().unwrap());
    }

    fn started(dir: &Path, tag: &str, timeout: u64) -> Arc<ChildManager> {
        let mut configs = HashMap::new();
        configs.insert(tag.to_string(), fake_config(dir, &[], timeout));
        Arc::new(ChildManager::new(configs, 300_000, timeout))
    }

    fn spawn_call(
        manager: &Arc<ChildManager>,
        server: &'static str,
        tool: &'static str,
        arguments: serde_json::Value,
    ) -> tokio::task::JoinHandle<Result<serde_json::Value, String>> {
        let manager = manager.clone();
        tokio::spawn(async move { manager.call_tool(server, tool, arguments).await })
    }

    #[tokio::test]
    async fn appels_stdio_simultanes_se_chevauchent() {
        let dir = fake_dir("mux");
        let manager = started(&dir, "fake-mux", 10);
        manager.start_server("fake-mux").await.unwrap();

        let began = Instant::now();
        let (first, second, third) = tokio::join!(
            manager.call_tool("fake-mux", "wait", serde_json::json!({})),
            manager.call_tool("fake-mux", "wait", serde_json::json!({})),
            manager.call_tool("fake-mux", "read_x", serde_json::json!({})),
        );
        let elapsed = began.elapsed();
        assert_eq!(first.unwrap()["content"][0]["text"], "wait");
        assert_eq!(second.unwrap()["content"][0]["text"], "wait");
        assert_eq!(third.unwrap()["content"][0]["text"], "x");
        assert!(
            elapsed >= Duration::from_millis(1000),
            "trop court : {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(1700),
            "appels en file : {:?}",
            elapsed
        );
        assert_eq!(lines(&dir.join("pids")).len(), 1, "un seul processus");

        // La requête roots/list émise par le serveur pendant read_x reçoit toujours une réponse.
        let answers = wait_lines(&dir.join("answers"), 1).await;
        assert!(answers[0].contains("\"id\":99999"), "{}", answers[0]);
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn un_changement_d_outils_annonce_est_relu() {
        let dir = fake_dir("outils-changes");
        let manager = started(&dir, "fake-outils", 10);
        let mut changes = subscribe_tools_changed();
        assert_eq!(manager.start_server("fake-outils").await.unwrap().len(), 3);
        manager.call_tool("fake-outils", "grow", serde_json::json!({})).await.unwrap();
        let announced = tokio::time::timeout(Duration::from_secs(5), changes.recv()).await.unwrap();
        assert_eq!(announced.as_deref(), Some("fake-outils"));
        let tools = manager.refresh_tools("fake-outils").await.unwrap().unwrap();
        assert!(tools.iter().any(|t| t.name == "extra"), "outil ajoute non relu");
        // Le serveur en marche rend désormais la liste relue.
        assert_eq!(manager.start_server("fake-outils").await.unwrap().len(), 4);
        assert!(manager.refresh_tools("inconnu").await.is_none());
        kill_fake(&dir);
    }

    #[tokio::test]
    async fn le_dossier_de_la_session_suit_chaque_appel_et_roots_list_le_donne() {
        let dir = fake_dir("racines");
        let manager = started(&dir, "fake-racines", 10);
        manager.start_server("fake-racines").await.unwrap();
        let call = |root: Option<&str>| {
            let manager = manager.clone();
            let root = root.map(PathBuf::from);
            async move {
                CALL_ROOT
                    .scope(root, async move {
                        manager
                            .call_tool("fake-racines", "where", serde_json::json!({}))
                            .await
                    })
                    .await
                    .unwrap()
            }
        };

        call(Some("/tmp/Mon projet")).await;
        let calls = wait_lines(&dir.join("calls"), 1).await;
        assert!(
            calls[0].contains(r#""_meta":{"com.soflution.mcphub/roots":[{"name":"Mon projet","uri":"file:///tmp/Mon%20projet"}]}"#),
            "{}",
            calls[0]
        );
        let answers = wait_lines(&dir.join("answers"), 1).await;
        assert!(
            answers[0].contains(r#""id":99998"#) && answers[0].contains("file:///tmp/Mon%20projet"),
            "{}",
            answers[0]
        );
        call(Some("/tmp/Mon projet")).await;
        call(Some("/tmp/autre")).await;
        let answers = wait_lines(&dir.join("answers"), 3).await;
        assert!(
            answers[2].contains("file:///tmp/autre"),
            "le dossier du dernier appel : {}",
            answers[2]
        );
        assert_eq!(
            lines(&dir.join("changed")).len(),
            2,
            "averti seulement quand le dossier change"
        );

        call(None).await;
        let calls = wait_lines(&dir.join("calls"), 4).await;
        assert!(
            !calls[3].contains("_meta"),
            "sans session connue, rien n'est ajouté : {}",
            calls[3]
        );
        manager.stop_all().await;
    }

    #[test]
    fn un_dossier_devient_une_racine_mcp() {
        assert_eq!(roots_value(None), serde_json::json!([]));
        assert_eq!(
            roots_value(Some(Path::new("/Users/moi/Projets/Café & co"))),
            serde_json::json!([{ "uri": "file:///Users/moi/Projets/Caf%C3%A9%20%26%20co", "name": "Café & co" }])
        );
        assert_eq!(call_root(), None, "hors d'un appel, aucun dossier");
    }

    #[tokio::test]
    async fn mort_de_l_enfant_deux_appels_en_vol_issue_inconnue() {
        let dir = fake_dir("mort");
        let manager = started(&dir, "fake-mort", 10);
        manager.start_server("fake-mort").await.unwrap();

        let first = spawn_call(&manager, "fake-mort", "hang", serde_json::json!({}));
        let second = spawn_call(&manager, "fake-mort", "hang", serde_json::json!({}));
        wait_lines(&dir.join("received"), 2).await;
        let began = Instant::now();
        kill_fake(&dir);

        for call in [first, second] {
            let err = call.await.unwrap().unwrap_err();
            assert!(is_unknown_outcome(&err), "{}", err);
        }
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "bloque {:?}",
            began.elapsed()
        );
        // Transmis : aucun des deux n'est rejoué, ni sur l'ancien processus ni sur un nouveau.
        assert_eq!(lines(&dir.join("pids")).len(), 1);
        assert_eq!(lines(&dir.join("received")).len(), 2);
        assert!(!manager.is_running("fake-mort").await);

        let ok = manager
            .call_tool("fake-mort", "read_x", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(ok["content"][0]["text"], "x");
        let received = lines(&dir.join("received"));
        assert_eq!(received.iter().filter(|l| l.starts_with("hang")).count(), 2);
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn mort_de_l_enfant_appel_non_ecrit_relance_appel_ecrit_non() {
        let dir = fake_dir("mort-file");
        let manager = started(&dir, "fake-file", 20);
        manager.start_server("fake-file").await.unwrap();

        // `stall` est transmis, puis le serveur ne lit plus : le gros appel reste bloqué en
        // cours d'écriture (aucune ligne complète reçue), le petit attend son tour derrière.
        let sent = spawn_call(&manager, "fake-file", "stall", serde_json::json!({}));
        wait_lines(&dir.join("received"), 1).await;
        let pad = "x".repeat(200_000);
        let writing = spawn_call(
            &manager,
            "fake-file",
            "read_x",
            serde_json::json!({ "pad": pad }),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let queued = spawn_call(&manager, "fake-file", "read_x", serde_json::json!({}));
        tokio::time::sleep(Duration::from_millis(200)).await;
        kill_fake(&dir);

        let err = sent.await.unwrap().unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        // Prouvés non envoyés : relancés une fois sur le processus redémarré.
        assert_eq!(writing.await.unwrap().unwrap()["content"][0]["text"], "x");
        assert_eq!(queued.await.unwrap().unwrap()["content"][0]["text"], "x");
        assert_eq!(lines(&dir.join("pids")).len(), 2, "un seul redemarrage");
        let received = lines(&dir.join("received"));
        assert_eq!(
            received.iter().filter(|l| l.starts_with("stall")).count(),
            1,
            "{:?}",
            received
        );
        assert_eq!(
            received.iter().filter(|l| l.starts_with("read_x")).count(),
            2,
            "{:?}",
            received
        );
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn delai_depasse_n_affecte_pas_l_autre_appel() {
        let dir = fake_dir("delai");
        let manager = started(&dir, "fake-delai", 2);
        manager.start_server("fake-delai").await.unwrap();

        let began = Instant::now();
        let (late, wait) = tokio::join!(
            manager.call_tool("fake-delai", "late", serde_json::json!({})),
            manager.call_tool("fake-delai", "wait", serde_json::json!({})),
        );
        let err = late.unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        assert_eq!(wait.unwrap()["content"][0]["text"], "wait");
        assert!(
            began.elapsed() < Duration::from_millis(2800),
            "{:?}",
            began.elapsed()
        );
        assert!(
            manager.is_running("fake-delai").await,
            "un delai depasse ne tue pas le serveur"
        );

        // La réponse tardive de `late` arrive vers 3 s : elle est ignorée, le processus sert
        // toujours et l'appel suivant reçoit bien sa propre réponse.
        tokio::time::sleep(Duration::from_millis(3300).saturating_sub(began.elapsed())).await;
        let ok = manager
            .call_tool("fake-delai", "read_x", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(ok["content"][0]["text"], "x");
        assert_eq!(lines(&dir.join("pids")).len(), 1);
        let pool = manager.pool("fake-delai").await.unwrap();
        assert!(!pool.busy());
        manager.stop_all().await;
    }

    #[tokio::test]
    async fn delai_depasse_avant_ecriture_rien_n_est_execute() {
        let dir = fake_dir("file");
        let manager = started(&dir, "fake-attente", 2);
        manager.start_server("fake-attente").await.unwrap();

        let sent = spawn_call(&manager, "fake-attente", "stall", serde_json::json!({}));
        wait_lines(&dir.join("received"), 1).await;
        let pad = "x".repeat(200_000);
        let writing = spawn_call(
            &manager,
            "fake-attente",
            "hang",
            serde_json::json!({ "pad": pad }),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let queued = spawn_call(&manager, "fake-attente", "hang", serde_json::json!({}));

        let err = sent.await.unwrap().unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        // Écriture commencée : le serveur peut encore lire la ligne plus tard, issue inconnue.
        let err = writing.await.unwrap().unwrap_err();
        assert!(is_unknown_outcome(&err), "{}", err);
        // Jamais écrit, et ne le sera jamais : sans risque à relancer.
        let err = queued.await.unwrap().unwrap_err();
        assert!(is_not_sent(&err), "{}", err);
        assert_eq!(
            lines(&dir.join("pids")).len(),
            1,
            "aucun redemarrage, aucun rejeu"
        );
        assert_eq!(lines(&dir.join("received")).len(), 1);
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
                    m.call_tool("fake-burst", "read_x", serde_json::json!({}))
                        .await
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
