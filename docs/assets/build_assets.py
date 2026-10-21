#!/usr/bin/env python3
"""Genere les visuels SVG du README (fond blanc, style moderne).
Usage : python3 docs/assets/build_assets.py
"""
import math
import os

OUT = os.path.dirname(os.path.abspath(__file__))

FONT = "Inter, -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "'SF Mono', ui-monospace, Menlo, Consolas, monospace"
INK, SUB, MUTED, LINE, SOFT = "#0F172A", "#475569", "#94A3B8", "#E2E8F0", "#F8FAFC"
AMBER, AMBER_SOFT, NAVY = "#F5B301", "#FFF7DB", "#1E1B4B"
GREEN, RED, BLUE, VIOLET, TEAL, ROSE = "#16A34A", "#DC2626", "#2563EB", "#7C3AED", "#0D9488", "#E11D48"


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def defs(extra=""):
    return f"""<defs>
  <filter id="sh" x="-10%" y="-10%" width="120%" height="140%">
    <feDropShadow dx="0" dy="10" stdDeviation="18" flood-color="#0F172A" flood-opacity="0.08"/>
  </filter>
  <filter id="sh2" x="-10%" y="-10%" width="120%" height="140%">
    <feDropShadow dx="0" dy="4" stdDeviation="8" flood-color="#0F172A" flood-opacity="0.06"/>
  </filter>
  <pattern id="dots" width="28" height="28" patternUnits="userSpaceOnUse">
    <circle cx="2" cy="2" r="1.4" fill="#E2E8F0"/>
  </pattern>
  <radialGradient id="glow" cx="50%" cy="50%" r="50%">
    <stop offset="0" stop-color="#FFE9A8" stop-opacity="0.9"/>
    <stop offset="1" stop-color="#FFFFFF" stop-opacity="0"/>
  </radialGradient>
  <radialGradient id="fade" cx="50%" cy="45%" r="70%">
    <stop offset="0" stop-color="#fff" stop-opacity="1"/>
    <stop offset="1" stop-color="#fff" stop-opacity="0"/>
  </radialGradient>
  <mask id="dotmask"><rect width="100%" height="100%" fill="url(#fade)"/></mask>
  <linearGradient id="amberbar" x1="0" x2="1">
    <stop offset="0" stop-color="#FBBF24"/><stop offset="1" stop-color="#F59E0B"/>
  </linearGradient>
  <linearGradient id="redbar" x1="0" x2="1">
    <stop offset="0" stop-color="#FCA5A5"/><stop offset="1" stop-color="#EF4444"/>
  </linearGradient>
  <marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
    <path d="M0,0 L10,5 L0,10 z" fill="#94A3B8"/>
  </marker>
  {extra}
</defs>"""


def svg(w, h, body, title):
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="{FONT}">
<title>{esc(title)}</title>
{defs()}
<rect width="{w}" height="{h}" fill="#FFFFFF"/>
{body}
</svg>
"""


def logo(x, y, size, spoke=5):
    s = size / 512.0
    cx = cy = 256
    R = 160
    pts = [(cx + R * math.sin(math.radians(a)), cy - R * math.cos(math.radians(a))) for a in range(0, 360, 45)]
    f = lambda p: f"{p[0]:.1f},{p[1]:.1f}"
    spokes = "".join(f'<line x1="{cx}" y1="{cy}" x2="{p[0]:.1f}" y2="{p[1]:.1f}"/>' for p in pts)
    nodes = "".join(
        f'<circle cx="{p[0]:.1f}" cy="{p[1]:.1f}" r="14" fill="{AMBER}"/><circle cx="{p[0]:.1f}" cy="{p[1]:.1f}" r="5" fill="#FFF7DB"/>'
        for p in pts
    )
    return f"""<g transform="translate({x},{y}) scale({s:.4f})">
  <g fill="none" stroke="{NAVY}" stroke-linecap="round">
    <circle cx="256" cy="256" r="122" stroke-opacity=".18" stroke-width="2"/>
    <circle cx="256" cy="256" r="92" stroke-opacity=".28" stroke-width="2"/>
    <circle cx="256" cy="256" r="64" stroke-opacity=".38" stroke-width="2"/>
    <polygon points="{' '.join(f(p) for p in pts)}" stroke-opacity=".45" stroke-width="3"/>
    <polygon points="{' '.join(f(pts[i]) for i in (1, 3, 5, 7))}" stroke-opacity=".22" stroke-width="2"/>
    <polygon points="{' '.join(f(pts[i]) for i in (0, 2, 4, 6))}" stroke-opacity=".14" stroke-width="2"/>
    <g stroke-width="{spoke}" stroke-opacity=".8">{spokes}</g>
  </g>
  {nodes}
  <circle cx="256" cy="256" r="46" fill="#FFF3C4"/>
  <circle cx="256" cy="256" r="32" fill="#FFE08A"/>
  <circle cx="256" cy="256" r="18" fill="{AMBER}"/>
</g>"""


def chip(x, y, text, color, w=None, h=48, size=20):
    w = w or int(len(text) * size * 0.56 + 64)
    return (
        f'<g transform="translate({x},{y})">'
        f'<rect width="{w}" height="{h}" rx="{h/2}" fill="#fff" stroke="{LINE}" filter="url(#sh2)"/>'
        f'<circle cx="26" cy="{h/2}" r="6" fill="{color}"/>'
        f'<text x="42" y="{h/2 + size*0.36:.1f}" font-size="{size}" font-weight="600" fill="{INK}">{esc(text)}</text>'
        f"</g>",
        w,
    )


def card(x, y, w, h, r=22, fill="#fff", stroke=LINE, shadow="sh"):
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{fill}" stroke="{stroke}" filter="url(#{shadow})"/>'


def text(x, y, s, size=20, weight=400, color=INK, anchor="start", family=None, extra=""):
    fam = f' font-family="{family}"' if family else ""
    return f'<text x="{x}" y="{y}" font-size="{size}" font-weight="{weight}" fill="{color}" text-anchor="{anchor}"{fam} {extra}>{esc(s)}</text>'


# ─── 1. Hero ─────────────────────────────────────────────────
def hero():
    W, H = 1600, 640
    b = [f'<rect width="{W}" height="{H}" fill="url(#dots)" mask="url(#dotmask)"/>']
    b.append(f'<circle cx="370" cy="320" r="300" fill="url(#glow)"/>')
    b.append(logo(140, 90, 460))
    b.append(f'<text x="690" y="268" font-size="132" font-weight="800" letter-spacing="-4"><tspan fill="{INK}">Mcp</tspan><tspan fill="{AMBER}">Hub</tspan></text>')
    b.append(text(694, 330, "One secure hub for all your MCP servers.", 40, 600, SUB))
    b.append(text(694, 378, "Cursor, Claude, Windsurf and VS Code see 3 tools instead of 928.", 26, 400, MUTED))
    x = 690
    y = 430
    for t, c in [("99.9% fewer tokens", AMBER), ("Sandboxed servers", GREEN), ("OAuth in the Keychain", BLUE)]:
        g, w = chip(x, y, t, c)
        b.append(g)
        x += w + 16
    x = 690
    y = 496
    for t, c in [("Anti rug-pull pinning", VIOLET), ("Code mode", TEAL), ("Single Rust binary", ROSE)]:
        g, w = chip(x, y, t, c)
        b.append(g)
        x += w + 16
    b.append(text(W - 48, H - 36, "v5.8  ·  MIT  ·  macOS", 18, 500, MUTED, "end"))
    return svg(W, H, "\n".join(b), "McpHub")


# ─── 2. Architecture ─────────────────────────────────────────
def architecture():
    W, H = 1600, 860
    b = [f'<rect width="{W}" height="{H}" fill="{SOFT}"/>']
    b.append(text(W / 2, 64, "How McpHub works", 40, 800, INK, "middle"))
    b.append(text(W / 2, 102, "Every client connects once. Every call goes through the same guards.", 22, 400, SUB, "middle"))

    # Clients
    clients = [("Cursor", "Streamable HTTP  /mcp/cursor"), ("Claude Desktop", "Streamable HTTP  /mcp"),
               ("Claude Code", "Streamable HTTP  /mcp"), ("Windsurf", "SSE  /sse"), ("VS Code", "stdio  McpHub stdio")]
    b.append(text(80, 168, "AI CLIENTS", 16, 700, MUTED, extra='letter-spacing="2"'))
    cy = 190
    client_mid = []
    for name, sub in clients:
        b.append(card(60, cy, 330, 96, 18, shadow="sh2"))
        b.append(f'<rect x="80" y="{cy+26}" width="44" height="44" rx="12" fill="{AMBER_SOFT}"/>')
        b.append(text(102, cy + 56, name[0], 22, 800, "#B45309", "middle"))
        b.append(text(140, cy + 44, name, 22, 700))
        b.append(text(140, cy + 72, sub, 13.5, 500, MUTED, family=MONO))
        client_mid.append(cy + 48)
        cy += 116

    # Hub
    hx, hy, hw, hh = 520, 160, 560, 640
    b.append(card(hx, hy, hw, hh, 28))
    b.append(f'<rect x="{hx}" y="{hy}" width="{hw}" height="96" rx="28" fill="{INK}"/>')
    b.append(f'<rect x="{hx}" y="{hy+60}" width="{hw}" height="36" fill="{INK}"/>')
    b.append(logo(hx + 24, hy + 14, 68, spoke=6))
    b.append(f'<text x="{hx+108}" y="{hy+52}" font-size="30" font-weight="800"><tspan fill="#fff">Mcp</tspan><tspan fill="{AMBER}">Hub</tspan></text>')
    b.append(text(hx + 108, hy + 80, "daemon  ·  127.0.0.1:24680  ·  one process", 16, 500, "#CBD5E1", family=MONO))
    steps = [
        ("Authenticate", "Bearer token, Host and Origin checks", BLUE),
        ("Apply the client profile", "Which servers and tools this client may see", VIOLET),
        ("Discover", "BM25 search: the client only sees 3 tools", AMBER),
        ("Guard the tool", "SHA-256 pinning and poisoning scan", ROSE),
        ("Check permissions", "Allow / deny rules per server and tool", GREEN),
        ("Run code mode", "Sandboxed JavaScript chains many calls", TEAL),
        ("Write the audit log", "Every call, secrets redacted", SUB),
    ]
    sy = hy + 132
    for i, (t, d, c) in enumerate(steps, 1):
        b.append(f'<circle cx="{hx+52}" cy="{sy+18}" r="18" fill="{c}"/>')
        b.append(text(hx + 52, sy + 25, str(i), 18, 800, "#fff", "middle"))
        if i < len(steps):
            b.append(f'<line x1="{hx+52}" y1="{sy+38}" x2="{hx+52}" y2="{sy+70}" stroke="{LINE}" stroke-width="3"/>')
        b.append(text(hx + 88, sy + 16, t, 21, 700))
        b.append(text(hx + 88, sy + 42, d, 16, 400, SUB))
        sy += 72

    # Connectors clients -> hub
    for m in client_mid:
        b.append(f'<path d="M390,{m} C455,{m} 455,{hy+hh/2} {hx-6},{hy+hh/2}" fill="none" stroke="#CBD5E1" stroke-width="2.5" marker-end="url(#arrow)"/>')

    # Servers
    sx, sw = 1210, 330
    b.append(text(sx + 20, 168, "MCP SERVERS", 16, 700, MUTED, extra='letter-spacing="2"'))
    groups = [
        ("Local servers", "run under the macOS sandbox", GREEN,
         ["supabase", "xcodebuild", "playwright", "context7", "firecrawl", "memory", "x", "+17 more"], 190),
        ("Remote servers", "native HTTP, OAuth in the Keychain", BLUE,
         ["stripe", "sentry", "cloudflare", "revenuecat", "tiktok-ads", "mobbin", "+5 more"], 500),
    ]
    for title, sub, color, names, gy in groups:
        gh = 270
        b.append(card(sx, gy, sw, gh, 22))
        b.append(f'<rect x="{sx}" y="{gy}" width="8" height="{gh}" rx="4" fill="{color}"/>')
        b.append(text(sx + 30, gy + 44, title, 22, 800))
        b.append(text(sx + 30, gy + 70, sub, 15, 500, SUB))
        px, py = sx + 30, gy + 98
        for n in names:
            w = int(len(n) * 9.6 + 30)
            if px + w > sx + sw - 20:
                px = sx + 30
                py += 46
            muted = n.startswith("+")
            b.append(f'<rect x="{px}" y="{py}" width="{w}" height="34" rx="17" fill="{SOFT if muted else "#fff"}" stroke="{LINE}"/>')
            b.append(text(px + w / 2, py + 23, n, 15, 600, MUTED if muted else INK, "middle", MONO))
            px += w + 10
        b.append(f'<path d="M{hx+hw+6},{hy+hh/2} C{hx+hw+70},{hy+hh/2} {sx-70},{gy+gh/2} {sx-8},{gy+gh/2}" fill="none" stroke="#CBD5E1" stroke-width="2.5" marker-end="url(#arrow)"/>')

    b.append(f'<rect x="{sx}" y="790" width="{sw}" height="46" rx="14" fill="{AMBER_SOFT}" stroke="#FDE68A"/>')
    b.append(text(sx + sw / 2, 820, "Secrets stay in the macOS Keychain", 17, 700, "#92400E", "middle"))
    return svg(W, H, "\n".join(b), "How McpHub works")


# ─── 3. Tokens ───────────────────────────────────────────────
def tokens():
    W, H = 1600, 560
    b = []
    b.append(text(80, 92, "Context spent on tool definitions", 40, 800))
    b.append(text(80, 132, "Measured on a real setup with 35 MCP servers and 928 tools.", 22, 400, SUB))
    # rows
    bx, bw = 80, 1060
    b.append(card(60, 172, 1120, 320, 24))
    b.append(text(100, 230, "Every server connected directly to the editor", 22, 700))
    b.append(text(1140, 230, "≈ 1,054,593 tokens", 22, 800, RED, "end"))
    b.append(f'<rect x="100" y="250" width="1040" height="44" rx="12" fill="url(#redbar)"/>')
    b.append(text(100, 322, "928 tool definitions loaded into every conversation, more than most context windows.", 17, 400, SUB))
    b.append(text(100, 392, "Through McpHub", 22, 700))
    b.append(text(1140, 392, "≈ 918 tokens", 22, 800, "#B45309", "end"))
    b.append(f'<rect x="100" y="412" width="1040" height="44" rx="12" fill="{SOFT}" stroke="{LINE}"/>')
    b.append(f'<rect x="100" y="412" width="14" height="44" rx="7" fill="url(#amberbar)"/>')
    b.append(text(130, 442, "3 tools: mcphub_search, mcphub_call, mcphub_run", 17, 600, SUB, family=MONO))
    # callout
    b.append(card(1220, 172, 320, 320, 24, fill=INK, stroke=INK))
    b.append(text(1380, 296, "-99.9%", 62, 800, AMBER, "middle"))
    b.append(text(1380, 350, "tokens per request", 22, 600, "#E2E8F0", "middle"))
    b.append(text(1380, 400, "Schemas are loaded", 18, 400, "#94A3B8", "middle"))
    b.append(text(1380, 426, "only for the tool in use.", 18, 400, "#94A3B8", "middle"))
    b.append(text(80, 528, "Estimate: JSON size of the tool definitions divided by 4 characters per token.", 16, 400, MUTED))
    return svg(W, H, "\n".join(b), "Token savings")


# ─── 4. Security ─────────────────────────────────────────────
def security():
    W, H = 1600, 900
    b = [f'<rect width="{W}" height="{H}" fill="{SOFT}"/>']
    b.append(text(W / 2, 74, "Defense in depth", 42, 800, INK, "middle"))
    b.append(text(W / 2, 114, "MCP servers are third-party code with your permissions. McpHub puts eight layers around them.", 22, 400, SUB, "middle"))
    layers = [
        ("Locked local API", "Bearer token, Host and Origin checks, no CORS.", "Web pages cannot reach the hub.", BLUE),
        ("Secrets in the Keychain", "Config files only hold ${NAME} references.", "No plain-text secrets on disk.", AMBER),
        ("Minimal environment", "Each server only receives its own variables,", "never every secret of the daemon.", TEAL),
        ("Tool pinning", "SHA-256 fingerprint of every tool definition.", "A silent change is quarantined.", VIOLET),
        ("Poisoning scan", "Invisible Unicode and injection phrases", "(\"ignore previous instructions\") are blocked.", ROSE),
        ("macOS sandbox", "No access to SSH keys, cloud tokens, browser", "cookies, shell profiles or LaunchAgents.", GREEN),
        ("Permissions and profiles", "Allow or deny per server and tool, per client.", "Read-only modes in one line.", "#EA580C"),
        ("Audit trail", "Every call is logged with redacted arguments,", "duration and decision.", SUB),
    ]
    cw, ch = 700, 150
    for i, (t, d1, d2, c) in enumerate(layers):
        col, row = i % 2, i // 2
        x = 80 + col * (cw + 40)
        y = 160 + row * (ch + 26)
        b.append(card(x, y, cw, ch, 22))
        b.append(f'<rect x="{x+28}" y="{y+34}" width="72" height="72" rx="20" fill="{c}" fill-opacity="0.12"/>')
        b.append(text(x + 64, y + 82, str(i + 1), 32, 800, c, "middle"))
        b.append(text(x + 128, y + 58, t, 25, 800))
        b.append(text(x + 128, y + 92, d1, 18, 400, SUB))
        b.append(text(x + 128, y + 118, d2, 18, 400, SUB))
    return svg(W, H, "\n".join(b), "Defense in depth")


# ─── 5. Code mode ────────────────────────────────────────────
def codemode():
    W, H = 1600, 640
    b = []
    b.append(text(80, 88, "Code mode: one call instead of 25", 40, 800))
    b.append(text(80, 128, "The agent sends a short program; only the final answer comes back into its context.", 22, 400, SUB))
    # left
    b.append(card(60, 170, 700, 410, 24))
    b.append(text(100, 222, "Without code mode", 24, 800))
    b.append(text(100, 252, "Each step is a round trip, each result fills the context.", 17, 400, SUB))
    y = 284
    rows = [("list_projects", "14 KB"), ("list_tables  ×12", "96 KB"), ("execute_sql  ×12", "48 KB"), ("…", "")]
    for name, size in rows:
        b.append(f'<rect x="100" y="{y}" width="620" height="52" rx="12" fill="{SOFT}" stroke="{LINE}"/>')
        b.append(text(124, y + 33, f"mcphub_call  {name}" if name != "…" else "…", 18, 600, INK, family=MONO))
        if size:
            b.append(text(696, y + 33, size, 18, 700, RED, "end"))
        y += 64
    b.append(text(100, 556, "25 round trips  ·  ~158 KB pushed into the model", 19, 700, RED))
    # right
    b.append(card(840, 170, 700, 410, 24))
    b.append(text(880, 222, "With mcphub_run", 24, 800))
    b.append(text(880, 252, "Runs in an isolated Node process: no files, no network.", 17, 400, SUB))
    b.append(f'<rect x="880" y="276" width="620" height="200" rx="14" fill="{INK}"/>')
    code = [
        ("const ps = await mcp.call('supabase', 'list_projects', {});", "#E2E8F0"),
        ("const out = [];", "#E2E8F0"),
        ("for (const p of ps) {", "#E2E8F0"),
        ("  const t = await mcp.call('supabase','list_tables',{project_id:p.id});", "#CBD5E1"),
        ("  out.push({ name: p.name, tables: t.length });", "#CBD5E1"),
        ("}", "#E2E8F0"),
        ("return out;", AMBER),
    ]
    cy = 308
    for line, col in code:
        b.append(text(904, cy, line, 13.5, 500, col, family=MONO, extra='xml:space="preserve"'))
        cy += 24
    b.append(f'<rect x="880" y="492" width="620" height="52" rx="12" fill="#F0FDF4" stroke="#BBF7D0"/>')
    b.append(text(904, 525, "1 call  ·  the answer only, a few hundred bytes", 19, 700, GREEN))
    return svg(W, H, "\n".join(b), "Code mode")


if __name__ == "__main__":
    for name, fn in [("hero", hero), ("architecture", architecture), ("tokens", tokens),
                     ("security", security), ("codemode", codemode)]:
        with open(os.path.join(OUT, f"{name}.svg"), "w") as f:
            f.write(fn())
        print(name)
