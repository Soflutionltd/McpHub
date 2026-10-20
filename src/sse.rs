/// SSE transport for MCP protocol.
/// Allows clients (Cursor, Claude Desktop) to connect via HTTP instead of stdio.
/// Sessions are managed via channels for zero-copy message passing.
///
/// Safety:
/// - TCP keepalive enabled to detect half-open connections
/// - Session reaper cleans stale sessions every 60s
/// - Write + flush errors both trigger session teardown
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

use crate::protocol::JsonRpcRequest;
use crate::proxy::ProxyServer;

/// A single SSE client session.
struct SseSession {
    tx: mpsc::Sender<String>,
    last_activity: Instant,
}

/// Manages all active SSE sessions.
pub struct SseManager {
    sessions: Arc<Mutex<HashMap<String, SseSession>>>,
}

/// Max time a session can be idle before reaper kills it (5 minutes).
const SESSION_TIMEOUT_SECS: u64 = 300;
/// Reaper interval.
const REAPER_INTERVAL_SECS: u64 = 60;
/// SSE keepalive interval.
const KEEPALIVE_INTERVAL_SECS: u64 = 15;

impl SseManager {
    pub fn new() -> Self {
        let manager = Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        // Start session reaper
        let sessions_ref = manager.sessions.clone();
        tokio::spawn(async move {
            session_reaper(sessions_ref).await;
        });
        manager
    }

    /// Handle GET /sse — establish long-lived SSE connection.
    /// Sends endpoint event, then streams responses until client disconnects.
    pub async fn handle_connect(&self, mut stream: TcpStream) {
        let session_id = generate_session_id();

        // Enable TCP keepalive to detect half-open connections.
        // OS will send probes after idle; dead peers detected in ~30-75s.
        configure_tcp_keepalive(&stream);

        // SSE response headers
        let headers = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: keep-alive\r\n\
             Access-Control-Allow-Origin: *\r\n\
             \r\n";

        if stream.write_all(headers.as_bytes()).await.is_err() {
            return;
        }

        // Send endpoint event — tells client where to POST messages
        let endpoint_event = format!(
            "event: endpoint\ndata: /message?sessionId={}\n\n",
            session_id
        );
        if write_and_flush(&mut stream, endpoint_event.as_bytes())
            .await
            .is_err()
        {
            return;
        }

        eprintln!("[McpHub][SSE] Client connected: {}", session_id);

        // Create channel for this session (bounded: backpressure if client is slow)
        let (tx, mut rx) = mpsc::channel::<String>(64);

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                session_id.clone(),
                SseSession {
                    tx,
                    last_activity: Instant::now(),
                },
            );
        }

        // Stream events until disconnect.
        // Keepalive every 15s to detect dead connections faster than TCP keepalive alone.
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(event) => {
                            if write_and_flush(&mut stream, event.as_bytes()).await.is_err() {
                                break;
                            }
                            touch_session(&self.sessions, &session_id).await;
                        }
                        None => break, // Channel dropped (session reaped or server shutdown)
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS)) => {
                    if write_and_flush(&mut stream, b": keepalive\n\n").await.is_err() {
                        break;
                    }
                    touch_session(&self.sessions, &session_id).await;
                }
            }
        }

        // Cleanup: remove session from map
        {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(&session_id);
        }
        // Explicitly shutdown the socket
        let _ = stream.shutdown().await;
        eprintln!("[McpHub][SSE] Client disconnected: {}", session_id);
    }

    /// Handle POST /message?sessionId=xxx — process JSON-RPC and send response via SSE.
    /// Returns HTTP response bytes (202 Accepted or error).
    pub async fn handle_message(
        &self,
        session_id: &str,
        body: &str,
        proxy: &Arc<ProxyServer>,
    ) -> Vec<u8> {
        // Parse JSON-RPC request
        let request: JsonRpcRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                return http_response(
                    400,
                    "Bad Request",
                    &format!("{{\"error\":\"Invalid JSON-RPC: {}\"}}", e),
                );
            }
        };

        let has_id = request.id.is_some();

        // Process through proxy
        let response = proxy.handle_request(request).await;

        // Send response through SSE stream
        if let Some(resp) = response {
            let json = match serde_json::to_string(&resp) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("[McpHub][SSE] Serialize error: {}", e);
                    return http_response(
                        500,
                        "Internal Server Error",
                        "{\"error\":\"Serialize failed\"}",
                    );
                }
            };

            let event = format!("event: message\ndata: {}\n\n", json);

            // Update last_activity and send via channel
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(session_id) {
                session.last_activity = Instant::now();
                // try_send: non-blocking, if channel full the client is too slow
                match session.tx.try_send(event) {
                    Ok(_) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        eprintln!(
                            "[McpHub][SSE] Session {} channel full, dropping message",
                            session_id
                        );
                        // Don't kill the session, just drop this message
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        drop(sessions);
                        // Session is dead, clean it up
                        self.sessions.lock().await.remove(session_id);
                        return http_response(410, "Gone", "{\"error\":\"Session closed\"}");
                    }
                }
            } else {
                return http_response(404, "Not Found", "{\"error\":\"Session not found\"}");
            }
        }

        // Return 202 Accepted for requests, 200 for notifications
        if has_id {
            http_response(202, "Accepted", "{\"ok\":true}")
        } else {
            http_response(200, "OK", "{\"ok\":true}")
        }
    }

    /// Get active session count.
    #[allow(dead_code)]
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

/// Extract sessionId from query string: /message?sessionId=xxx
pub fn extract_session_id(path: &str) -> Option<String> {
    let query = path.split('?').nth(1)?;
    for param in query.split('&') {
        if let Some(val) = param.strip_prefix("sessionId=") {
            return Some(val.to_string());
        }
    }
    None
}

/// Write bytes + flush. Returns Err if either fails.
async fn write_and_flush(stream: &mut TcpStream, data: &[u8]) -> Result<(), ()> {
    if stream.write_all(data).await.is_err() {
        return Err(());
    }
    if stream.flush().await.is_err() {
        return Err(());
    }
    Ok(())
}

/// Configure TCP keepalive on the socket to detect dead peers.
fn configure_tcp_keepalive(stream: &TcpStream) {
    use std::time::Duration;
    let sock_ref = socket2::SockRef::from(stream);
    let mut ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        ka = ka.with_retries(3);
    }
    let _ = sock_ref.set_tcp_keepalive(&ka);
    let _ = sock_ref.set_nodelay(true);
}

/// Periodically reap stale sessions (no successful reads/writes for SESSION_TIMEOUT_SECS).
/// Dropping the sender half of the channel causes the SSE loop to break.
async fn session_reaper(sessions: Arc<Mutex<HashMap<String, SseSession>>>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(REAPER_INTERVAL_SECS)).await;
        let mut map = sessions.lock().await;
        let stale: Vec<String> = map
            .iter()
            .filter(|(_, s)| s.last_activity.elapsed().as_secs() > SESSION_TIMEOUT_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            map.remove(id);
            eprintln!("[McpHub][SSE] Reaped stale session: {}", id);
        }
    }
}

async fn touch_session(sessions: &Arc<Mutex<HashMap<String, SseSession>>>, session_id: &str) {
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(session_id) {
        session.last_activity = Instant::now();
    }
}

/// Generate a random session ID (no uuid crate needed).
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", nanos, pid, count)
}

fn http_response(status: u16, status_text: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        status_text,
        body.len(),
        body
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn touch_session_refreshes_activity_for_keepalive() {
        let (tx, _rx) = mpsc::channel::<String>(1);
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "session-1".to_string(),
            SseSession {
                tx,
                last_activity: Instant::now()
                    - std::time::Duration::from_secs(SESSION_TIMEOUT_SECS + 10),
            },
        )])));

        touch_session(&sessions, "session-1").await;

        let sessions = sessions.lock().await;
        let elapsed = sessions
            .get("session-1")
            .expect("session should still exist")
            .last_activity
            .elapsed();

        assert!(elapsed.as_secs() < SESSION_TIMEOUT_SECS);
    }
}
