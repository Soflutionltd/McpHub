# McpHub v4.0.0 — Improvement Roadmap for Cursor Agent

## Project Overview

McpHub is a single Rust binary (~1MB) that acts as an MCP (Model Context Protocol) proxy between AI editors (Cursor, Claude Desktop, Windsurf) and multiple MCP servers. It exposes 2 meta-tools (discover + execute) instead of 200+ tool definitions, saving ~99% context tokens. It includes a BM25 search engine, a web dashboard, health monitoring with native OS notifications, and as of v4.0.0, SSE transport for persistent connections that survive editor crashes.

## File Locations

- **Source code:** `/tmp/mcp-on-demand-check/src/`
- **GitHub repo:** `https://github.com/Soflution1/McpHub`
- **Binary (installed):** `~/.local/bin/McpHub`
- **Config:** `~/.McpHub/config.json` (27 servers configured)
- **Cache:** `~/.McpHub/schema-cache.json` (460 tools indexed)
- **Logs:** `~/.McpHub/mcphub.log`
- **LaunchAgent:** `~/Library/LaunchAgents/com.soflution.mcphub.plist`
- **Cursor MCP config:** `~/.cursor/mcp.json` (uses `"url": "http://127.0.0.1:24680/sse"`)
- **CI workflow:** `.github/workflows/release.yml`

## Source Files Architecture

```
src/
├── main.rs        — CLI entry point, commands (serve, install, generate, dashboard, search, status)
├── proxy.rs       — Core proxy: init(), handle_request(), stdio_loop(), discover/execute logic, BM25 routing
├── sse.rs         — SSE transport: SseManager, sessions, channels, keepalive, reaper
├── dashboard.rs   — HTTP server (port 24680): dashboard HTML/CSS/JS + REST API + SSE/message routes
├── child.rs       — Child process manager: spawn MCP servers, stdio communication, retry, idle reaper
├── protocol.rs    — JSON-RPC 2.0 types, MCP initialize/capabilities structs
├── search.rs      — BM25 search engine: tokenizer, IDF/TF, index build, ranked search
├── cache.rs       — Schema cache: load/save/repair cache file
├── config.rs      — Config parsing: auto-detect from ~/.McpHub, ~/.cursor, Claude Desktop configs
├── health.rs      — Health monitor: periodic pings, auto-restart with backoff, native OS notifications
├── install.rs     — Cross-platform auto-start: macOS LaunchAgent, Linux systemd, Windows Registry
```

## Current State (v4.0.0 — working, deployed)

- SSE transport working (GET /sse + POST /message)
- LaunchAgent installed, daemon running persistently
- TCP keepalive on SSE sockets (15s probe, 5s interval, 3 retries)
- Session reaper every 60s (5min timeout)
- Read timeout 10s on HTTP connections (Slowloris protection)
- try_send on SSE channels (non-blocking)
- CI builds for macOS ARM, macOS Intel, Linux amd64
- Dashboard with add/edit/toggle/rebuild servers

## Reliability pass (branch `wip/fiabilite`, 2026-10-28)

Invariants now enforced by tests. Do not regress them:

- A `tools/call` that may have reached the child (timeout after send, child died after send) is never
  replayed, neither by the daemon nor by the Node client. Only a call proven unsent is retried. Reads
  (`prompts/get`, `resources/read`, `ping`) may be retried after a restart.
- Every failure carries a status: `ok`, `error`, `not_executed`, `unknown_outcome`,
  `confirmation_required` (`_meta.mcphub.status`, `_mcphub.status`, `error.data.status`).
- Config is Missing, Invalid or Valid. An invalid file is never overwritten and never replaces the last
  valid config on reload. Writes are atomic (new 600 file, fsync, rename).
- Unknown sandbox or permission values are errors, never a silent "off" or "allow".
- Agent installs (`registry_install`) ask for confirmation unless a rule says otherwise.
- The response cache skips errors, is bounded in size, and is invalidated per server on mutating calls
  and on every restart (epoch).
- Only implemented MCP revisions are announced (`src/protocol.rs`).
- `McpHub update` never downgrades, requires the published SHA-256, checks `--version` of the new binary
  and rolls back on failure.
- Automatic updates: only from a `latest.json` signed with the minisign key whose public half is
  `UPDATE_PUBLIC_KEY` in `src/update.rs` (private half in 1Password and the GitHub `release`
  environment). macOS binaries must carry the Developer ID of team TB8CC687M3. Gradual rollout (10%, then
  everyone after 24 h), restart only when `in_flight() == 0`, rollback after 3 failed starts. Never in
  debug builds, named instances or a cargo `target/` directory.

Next, not done yet:

- Persistent stdio reader per child: real cancellation (`notifications/cancelled` forwarded) and several
  requests in flight per server.
- Per-client credentials instead of one shared bearer token.
- Paginated or resource-backed large results.
- Native MCP revision 2026-07-28 (stateless, `server/discover`).
- `GET /api/servers` returns env values: mask them. `McpHub install` prints the bearer token.
- Toolguard and Code Mode settings are still parsed leniently.
- Dashboard and registry read-modify-write `config.json` without a lock.
- A profile change does not invalidate the response cache.
- Reproducible benchmark protocol.

## IMPROVEMENTS TO IMPLEMENT

Work through these in order. Each one should be a separate git commit with a clear message.

---

### TIER 1 — Active Bugs (do these first)

#### 1. Complete POST body reading (Content-Length)
**File:** `src/dashboard.rs`, function `handle_connection`
**Problem:** `stream.read(&mut buf)` reads up to 64KB in one call. A large POST body (tools/call with big arguments) can be truncated, causing JSON parse errors.
**Fix:** After the first read, check Content-Length header. If body is incomplete, keep reading in a loop until we have all bytes or timeout. Example:
```rust
// After initial read, parse Content-Length from headers
// If body.len() < content_length, read more:
while body_received < content_length {
    let n = timeout(Duration::from_secs(10), stream.read(&mut buf)).await??;
    // append to buffer
}
```

#### 2. CORS OPTIONS handler
**File:** `src/dashboard.rs`, in the routing logic of `handle_connection`
**Problem:** Some HTTP clients send an OPTIONS preflight before POST. Currently returns 404.
**Fix:** Add before the other route checks:
```rust
if req.method == "OPTIONS" {
    let resp = b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Max-Age: 86400\r\nContent-Length: 0\r\n\r\n";
    stream.write_all(resp).await;
    return;
}
```

#### 3. Graceful shutdown
**File:** `src/main.rs` and `src/proxy.rs`
**Problem:** When daemon receives SIGTERM/SIGINT, child MCP server processes are orphaned (zombies).
**Fix:** Add a signal handler using `tokio::signal`:
```rust
let proxy_shutdown = proxy.clone();
tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    eprintln!("[McpHub] Shutting down...");
    proxy_shutdown.shutdown().await;
    std::process::exit(0);
});
```
Add `pub async fn shutdown(&self)` to ProxyServer that calls `self.child_manager.stop_all().await`.
Also handle SIGTERM on Unix:
```rust
#[cfg(unix)]
{
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    // ...
}
```

---

### TIER 2 — Security & Stability

#### 4. Auth token on HTTP endpoints
**File:** `src/sse.rs` and `src/dashboard.rs`
**Problem:** Any local process can call MCP tools via port 24680. If exposed on network, API keys are accessible.
**Fix:**
- On first run, generate a random token and save it to `~/.McpHub/auth-token`
- Check `Authorization: Bearer <token>` header on `/sse` and `/message` endpoints
- Dashboard routes can stay unauthenticated (read-only, local only)
- `McpHub install` should print the token
- Cursor config becomes: `"url": "http://127.0.0.1:24680/sse"` with `"headers": {"Authorization": "Bearer <token>"}`

#### 5. Retry on tools/call failure (superseded)
**Superseded by the reliability pass:** retrying a call that may already have run executes it twice
(an email sent twice, a payment made twice). Only an unsent call is retried.
**File:** `src/child.rs`, function `call_tool`
**Problem:** If a child server crashes during a tool call, user gets an error. Server could be restarted and call retried.
**Fix:** In `call_tool`, if the result is a write/read error (not an MCP error response), restart the server and retry once:
```rust
match result {
    Err(e) if is_connection_error(&e) => {
        self.restart_server(server_name).await?;
        // retry once
        send_request(proc, "tools/call", params).await
    }
    other => other
}
```

#### 6. Hot reload config (not just cache)
**File:** `src/proxy.rs`, function `cache_watcher` — extend it or create a new watcher
**Problem:** Currently only `schema-cache.json` is watched. If user edits `config.json` (adds/removes servers), nothing happens until restart.
**Fix:** Watch `config.json` too. On change, re-parse config, diff with current, stop removed servers, add new ones to the configs map.

---

### TIER 3 — Tests

#### 7. Unit tests
**Create:** `src/search.rs` — add `#[cfg(test)] mod tests` at the bottom
- Test BM25 indexing: build index with known tools, search returns correct ranking
- Test tokenizer: camelCase splitting, stopword removal
- Test empty index search

**Create:** `src/protocol.rs` — test JSON serialization/deserialization
- Test JsonRpcRequest parsing
- Test JsonRpcResponse::success and ::error serialization
- Test Capabilities serialization matches expected JSON

**Create:** `src/config.rs` — test config parsing
- Test `is_self` detection (skip McpHub in its own config)
- Test `parse_servers` with various JSON formats
- Test disabled servers are skipped

#### 8. Integration tests
**Create:** `tests/integration.rs`
- Start McpHub in serve mode, connect via SSE, send initialize, verify response
- Test discover returns results from cache
- Test execute routes to correct child server
- Test session cleanup after disconnect
- Test keepalive on SSE stream

---

### TIER 4 — CLI Tools

#### 9. `McpHub doctor`
**File:** Create `src/doctor.rs`, add command to `src/main.rs`
**Purpose:** Full diagnostic of the installation.
**Checks:**
- Binary version and location
- Config file exists and is valid JSON
- Cache file exists, age, number of servers/tools
- Port 24680 availability (or if daemon is already running)
- For each server in config: check if command exists (which node, which npx, which uvx)
- For each server: check required env vars are set
- Ping running daemon if present
- Check LaunchAgent/systemd/registry status
- Report disk usage of ~/.McpHub/
**Output:** Green checkmarks / red crosses with clear messages.

#### 10. `McpHub logs`
**File:** Create `src/logs.rs`, add command to `src/main.rs`
**Purpose:** Tail daemon logs in real time with optional filtering.
**Usage:** `McpHub logs`, `McpHub logs --server github`, `McpHub logs --level error`
**Implementation:** Read `~/.McpHub/mcphub.log` and tail -f with colored output. Parse `[McpHub][LEVEL]` and `[McpHub][SERVER]` prefixes for filtering.

#### 11. `McpHub add`
**File:** Create `src/add.rs`, add command to `src/main.rs`
**Purpose:** Interactive CLI to add a server.
**Flow:**
```
$ McpHub add
Server name: github
Command: npx
Arguments: -y @modelcontextprotocol/server-github
Environment variables (KEY=VALUE, empty to finish):
  GITHUB_TOKEN=ghp_xxx

Testing connection... ✓ 28 tools found
Added 'github' to ~/.McpHub/config.json
Run 'McpHub generate' to rebuild cache.
```

#### 12. `McpHub benchmark`
**File:** Create `src/benchmark.rs`, add command to `src/main.rs`
**Purpose:** Measure performance of each server.
**Output:**
```
Server          | Start    | Ping   | Tools | RAM
----------------|----------|--------|-------|------
github          | 1.2s     | 45ms   | 28    | 12MB
supabase        | 0.8s     | 32ms   | 18    | 8MB
cloudflare      | 2.1s     | 67ms   | 15    | 15MB
```

---

### TIER 5 — CI & Packaging

#### 13. Windows build in CI
**File:** `.github/workflows/release.yml`
**Add to matrix:**
```yaml
- target: x86_64-pc-windows-msvc
  os: windows-latest
  asset: McpHub-windows-amd64.zip
```
Note: `notify-rust` needs different handling on Windows. Should compile fine but test the notification code path.

#### 14. Linux ARM build
**File:** `.github/workflows/release.yml`
**Add to matrix:**
```yaml
- target: aarch64-unknown-linux-gnu
  os: ubuntu-latest
  asset: McpHub-linux-arm64.tar.gz
```
Needs `cross` or `cargo-cross` for cross-compilation, or use a `linux/arm64` runner.

#### 15. Update install.sh
**File:** `install.sh` at repo root
**Update to:**
- After build, run `McpHub generate` if cache doesn't exist
- Offer to run `McpHub install` for auto-start
- Print the Cursor SSE config to copy

---

### TIER 6 — Dashboard Enhancements

#### 16. Metrics collection
**File:** `src/proxy.rs` — add a Metrics struct behind Arc<Mutex<>>
**Track per server:** call count, total latency, error count, last call time, last error
**Track global:** total requests, active SSE sessions, uptime
**Expose:** `GET /api/metrics` endpoint in dashboard.rs

#### 17. Metrics visualization in dashboard
**File:** `src/dashboard.rs` — in the HTML/JS section
**Add:** A "Metrics" tab showing:
- Bar chart: calls per server (last hour)
- Line chart: latency over time
- Error rate badges per server
- Uptime counter
Use inline SVG or a tiny JS chart lib (no external deps).

#### 18. Live logs in dashboard
**File:** `src/dashboard.rs`
**Add:** A "Logs" tab that streams logs via a separate SSE endpoint `/api/logs-stream`
- Capture eprintln output into a ring buffer
- Stream to connected dashboard clients
- Filter controls in the UI

---

### TIER 7 — Protocol Completeness

#### 19. Resources aggregation
**File:** `src/proxy.rs`
**Currently:** `resources/list` returns empty. `resources/read` returns error.
**Fix:** When a child server declares resources capability, proxy should aggregate all resources from all running servers and expose them. Prefix resource URIs with server name to avoid collisions.

#### 20. Prompts aggregation
**File:** `src/proxy.rs`
**Same as resources:** aggregate prompts from child servers, prefix names.

#### 21. Cancellation forwarding
**File:** `src/proxy.rs` and `src/child.rs`
**Problem:** If the client sends `notifications/cancelled` with a request ID, the proxy ignores it. The child server keeps working.
**Fix:** Track which request ID is being handled by which child server. On cancel notification, forward `notifications/cancelled` to the appropriate child.

#### 22. MCP logging forwarding
**File:** `src/child.rs`
**Problem:** Child servers may send `notifications/message` (log messages). These are currently ignored (skipped as notifications without ID in send_request_inner).
**Fix:** Capture these notifications and forward them to the SSE client, or aggregate them for the dashboard logs.

---

### TIER 8 — Advanced Features

#### 23. Connection pooling
**File:** `src/child.rs`
**Problem:** One instance per server, mutex on stdout means sequential calls only.
**Fix:** Allow spawning N instances of the same server (configurable: `"pool": 3` in config). Round-robin or least-connections routing. Each instance has its own stdin/stdout/process.

#### 24. Self-update
**File:** Create `src/update.rs`
**Purpose:** `McpHub update` checks GitHub Releases API for latest version, downloads the right binary for the current OS/arch, replaces itself, restarts daemon.

#### 25. Export/Import
**File:** Create `src/export.rs`
**Purpose:**
- `McpHub export > setup.json` — exports config + encrypted env vars
- `McpHub import setup.json` — imports config, prompts for secrets
Useful for team sharing or machine migration.

#### 26. Protocol version negotiation
**File:** `src/child.rs`, in `try_start_server`
**Problem:** Hardcoded `protocolVersion: "2024-11-05"` in initialize. Some servers may support newer versions.
**Fix:** Parse the server's initialize response, store its supported version, adapt message format accordingly.

---

## Build & Test Commands

```bash
cd /tmp/mcp-on-demand-check

# Build
cargo build --release

# Run tests (once you add them)
cargo test

# Deploy locally
rm -f ~/.local/bin/McpHub
cp target/release/McpHub ~/.local/bin/McpHub
chmod +x ~/.local/bin/McpHub
xattr -cr ~/.local/bin/McpHub  # macOS only

# Restart daemon
launchctl unload ~/Library/LaunchAgents/com.soflution.mcphub.plist
launchctl load ~/Library/LaunchAgents/com.soflution.mcphub.plist

# Check logs
tail -f ~/.McpHub/mcphub.log

# Test SSE
curl -N http://127.0.0.1:24680/sse

# Test message (replace SESSION_ID)
curl -X POST "http://127.0.0.1:24680/message?sessionId=SESSION_ID" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'

# Git workflow
git add -A && git commit -m "description" && git push origin main
```

## Rules

- Keep the binary small. No heavy dependencies. Think twice before adding a crate.
- Every feature gets a commit with a clear message.
- Don't break existing functionality. The SSE transport, stdio mode, dashboard, and all CLI commands must keep working.
- Rust idioms: use Result instead of panic, handle errors gracefully, prefer &str over String where possible.
- Test your changes: build, run, verify manually before committing.
- The binary name is `McpHub` (capital M, capital H). Don't change it.
- Dashboard HTML/CSS/JS is embedded in dashboard.rs as string literals. No external files.
- Version in Cargo.toml should be bumped for significant changes.
