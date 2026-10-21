<p align="center">
  <img src="static/banner.png" alt="McpHub" width="900"/>
</p>

<p align="center">
  <strong>One proxy to rule all your MCP servers.</strong><br>
  <sub>SSE transport · Auth · Connection pooling · Dashboard with metrics · ~99% context savings · Zero dependencies</sub>
</p>

<p align="center">
  <a href="#install"><img src="https://img.shields.io/badge/install-30s-brightgreen" alt="Install"/></a>
  <img src="https://img.shields.io/badge/language-Rust-orange" alt="Rust"/>
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT"/>
  <img src="https://img.shields.io/badge/binary-~1MB-yellow" alt="Binary size"/>
  <img src="https://img.shields.io/badge/v5.0-latest-green" alt="v5.0"/>
</p>

---

## What is McpHub?

McpHub is a single Rust binary that sits between your AI editor (Cursor, Claude Desktop, Windsurf) and all your MCP servers. Instead of loading 20+ servers with 200+ tool definitions into every prompt (~20,000 tokens), the editor sees only 2 tools: `discover` and `execute`. Token savings: **~99%**.

McpHub runs as a persistent daemon. Your editor connects via SSE URL instead of spawning a process. If Cursor crashes or restarts, McpHub stays alive and reconnects instantly. No manual refresh, no lost state.

## Install

### From source

```bash
git clone https://github.com/Soflution1/McpHub.git
cd McpHub
./install.sh
```

The install script builds the release binary (~1MB), installs to `~/.local/bin/McpHub`, codesigns for macOS, and generates the tool cache.

### Pre-built binaries

Download from [GitHub Releases](https://github.com/Soflution1/McpHub/releases) for macOS (ARM/Intel), Linux (amd64/arm64), and Windows.

### Setup (SSE mode, recommended)

```bash
# 1. Generate tool cache (one-time, ~60s)
McpHub generate

# 2. Install as auto-start daemon
McpHub install

# 3. Configure your editor
```

**Cursor** (`~/.cursor/mcp.json`):
```json
{
  "mcpServers": {
    "McpHub": {
      "url": "http://127.0.0.1:24680/sse",
      "headers": {
        "Authorization": "Bearer <your-token>"
      }
    }
  }
}
```

Your auth token is auto-generated on first run and stored in `~/.McpHub/auth-token`. All SSE and API endpoints require it.

### Setup (stdio mode)

If you prefer the editor to manage the process lifecycle:

```json
{
  "mcpServers": {
    "McpHub": {
      "command": "/Users/you/.local/bin/McpHub"
    }
  }
}
```

In this mode, McpHub also starts the HTTP server on `:24680` in the background.

## How It Works

```
Cursor (sees only 2 tools: discover + execute)
    ↓ SSE (http://127.0.0.1:24680/sse)
McpHub daemon (BM25 search, auth, connection pool)
    ↓ stdio (pooled connections)
Your MCP servers (spawned on demand, killed when idle)
```

### Discover mode (default)

1. LLM calls `discover("send email")`
2. McpHub searches across all tools using BM25 ranking
3. Returns matching tools with full schemas
4. LLM calls `execute("resend", "send-email", {to: "...", ...})`
5. McpHub routes to the right server, calls the tool, returns result

Server names are resolved case-insensitively.

### Passthrough mode

All tools exposed directly with `server__tool` prefix. Full visibility, higher token cost. Set `"mode": "passthrough"` in settings.

## Dashboard

Open `http://127.0.0.1:24680` or run `McpHub dashboard`.

Features:
- Add/edit/enable/disable servers with syntax-highlighted JSON
- Real-time metrics: calls per server, latency, error rates, uptime
- Live log streaming with server and level filters
- Rebuild cache in one click
- Token savings counter

## Transport Modes

| Mode | Command | Editor config | Survives editor crash |
|---|---|---|---|
| **Streamable HTTP (recommended)** | `McpHub serve` or `McpHub install` | `"url": "http://127.0.0.1:24680/mcp"` (or `/mcp/<profile>`) | Yes |
| **SSE (legacy)** | `McpHub serve` or `McpHub install` | `"url": "http://127.0.0.1:24680/sse"` (optional `?profile=<profile>`) | Yes |
| **stdio** | `McpHub` (default) | `"command": "/path/to/McpHub"` | No |

SSE uses TCP keepalive (15s probe, 5s interval, 3 retries), a session reaper for stale connections, and non-blocking sends to prevent slow clients from blocking the server.

## CLI

```bash
McpHub                  # Start proxy (stdio + HTTP server on :24680)
McpHub serve            # Start HTTP-only server (SSE daemon)
McpHub install          # Register auto-start at login
McpHub uninstall        # Remove auto-start
McpHub generate         # Rebuild tool cache
McpHub dashboard        # Open web dashboard
McpHub status           # Show detected servers and cache info
McpHub search "git"     # Test BM25 search
McpHub doctor           # Full diagnostic (binary, config, cache, ports, daemon)
McpHub logs             # Tail daemon logs (--server, --level filters)
McpHub add              # Interactive wizard to add a server
McpHub benchmark        # Measure start time, ping latency, tool count, RAM
McpHub export           # Export config as encrypted bundle for sharing
McpHub import <file>    # Import config bundle
McpHub update           # Self-update from GitHub Releases
McpHub vault            # Secrets in the macOS Keychain (status, list, set, get, rm, migrate)
McpHub policy           # Tool permissions (allow / confirm / deny)
McpHub audit            # Audit log of every tool call
McpHub pins             # Tool pinning: list, status, approve SERVER [TOOL], reset SERVER, scan
McpHub version          # Show version
```

## Security

McpHub generates a random auth token on first run, stored in `~/.McpHub/auth-token` (mode 600).

- `/mcp`, `/sse`, `/message` and every `/api/*` route require `Authorization: Bearer <token>`. The dashboard (`/`) gets an `HttpOnly; SameSite=Strict` session cookie instead, so its JavaScript never sees the token and no third-party site can reuse it.
- Requests whose `Host` is not `127.0.0.1` / `localhost` / `[::1]` are refused (DNS rebinding), as are requests carrying a foreign `Origin`. No CORS headers are sent: browsers cannot call the hub from another site.
- Secrets live in the macOS Keychain (`McpHub vault`), referenced as `${NAME}` in `config.json`.
- Permissions per server/tool (`settings.permissions`: allow / confirm / deny), with optional auto-classification of destructive tools (by name and by the MCP `destructiveHint` annotation). Every call is written to `~/.McpHub/audit.jsonl` with secrets redacted.

### Tool pinning and poisoning detection

The first time McpHub sees a server, it records a SHA-256 fingerprint of every tool (name, description, schema, annotations) in `~/.McpHub/tool-pins.json`. If a server later changes a tool definition (a "rug pull", easy with `npx -y package@latest`), that tool is quarantined: hidden from the catalog and refused on call, until you check it and run `McpHub pins approve SERVER [TOOL]`.

Descriptions are also scanned for invisible Unicode characters and classic injection phrases ("ignore previous instructions", "do not tell the user", `~/.ssh/id_rsa`, `mcp.json`...). `McpHub pins scan` checks the whole cache without changing anything.

```json
"settings": {
  "toolSecurity": { "pinning": "block", "scan": "block", "newTools": "warn" }
}
```

Each value is `block`, `warn` or `off`. `newTools: "block"` also holds back tools that appear on an already approved server.

### Per-server tool filters

```json
"mcpServers": {
  "supabase": {
    "command": "npx", "args": ["-y", "@supabase/mcp-server-supabase"],
    "tools": { "exclude": ["delete_*", "pause_project"] },
    "toolDescriptions": { "list_projects": "List my Supabase projects" }
  },
  "github": { "command": "...", "readOnly": true }
}
```

`tools.include` / `tools.exclude` take globs; `readOnly: true` only exposes tools that read (`readOnlyHint`, or a read verb such as get/list/search, never a destructive name).

### Profiles per client

```json
"settings": {
  "profiles": {
    "cursor": { "servers": ["supabase", "git*", "context7"], "excludeTools": ["supabase/execute_sql"] },
    "review": { "readOnly": true }
  }
}
```

Use `http://127.0.0.1:24680/mcp/cursor`, `/sse?profile=cursor`, or `MCPHUB_PROFILE=cursor` in stdio mode. Each client then only sees, searches and calls what its profile allows.

If a server crashes during a `tools/call`, McpHub auto-restarts it and retries the call once before returning an error.

## Connection Pooling

McpHub can maintain multiple instances of a server for parallel request handling. Configure per server:

```json
{
  "servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_xxx" },
      "pool_size": 3
    }
  }
}
```

Requests are routed round-robin across pool instances. Default pool size is 1.

## Protocol Support

McpHub implements the full MCP protocol as a proxy:

- **Tools**: `tools/list`, `tools/call` (aggregated from all servers)
- **Resources**: `resources/list`, `resources/read` (aggregated)
- **Prompts**: `prompts/list`, `prompts/get` (aggregated)
- **Cancellation**: `notifications/cancelled` forwarded to child servers
- **Logging**: `notifications/message` captured and forwarded
- **Version negotiation**: Adapts to each server's supported protocol version
- **Tool metadata**: `title`, `annotations` and `outputSchema` are relayed as declared by each server
- **Pagination**: child `tools/list` pages (`nextCursor`) are all read
- **Child logs**: each server's stderr goes to `~/Library/Logs/McpHub/<server>.stderr.log`

## Configuration

Config lives in `~/.McpHub/config.json`:

```json
{
  "servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_xxx" }
    }
  },
  "settings": {
    "mode": "discover",
    "mediaProfile": "safe",
    "idleTimeout": 300,
    "health": {
      "checkInterval": 30,
      "autoRestart": true,
      "notifications": true
    }
  }
}
```

`mediaProfile` is explicit and opt-in:

- `safe`: default, only passes image formats known to be widely decoded by MCP clients
- `cursor-rich`: also passes `image/webp` inline for richer clients
- `raw`: disables inline media normalization completely

### Health monitoring

McpHub pings running servers periodically. If one crashes, you get a native OS notification and the server is auto-restarted with exponential backoff (up to 3 attempts).

### Hot reload

Edit `config.json` while the daemon is running. McpHub detects changes, diffs the config, stops removed servers, and starts new ones without a restart.

## Performance

| Metric | Value |
|---|---|
| Binary size | ~1 MB |
| Startup | <5 ms |
| BM25 search (460 tools) | <0.01 ms |
| Context token savings | ~99% |
| RAM usage | ~5 MB |
| SSE keepalive overhead | ~40 bytes/15s |
| Runtime dependencies | **None** |

## Cross-platform auto-start

`McpHub install` detects your OS and creates the appropriate auto-start entry:

| OS | Method | Location |
|---|---|---|
| macOS | LaunchAgent | `~/Library/LaunchAgents/com.soflution.mcphub.plist` |
| Linux | systemd user service | `~/.config/systemd/user/mcphub.service` |
| Windows | Registry Run key | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |

Pre-built binaries available for macOS ARM, macOS Intel, Linux amd64, Linux arm64, and Windows x64.

## Uninstall

```bash
McpHub uninstall
rm ~/.local/bin/McpHub
rm -rf ~/.McpHub
```

## License

MIT - [SOFLUTION LTD](https://soflution.com)
