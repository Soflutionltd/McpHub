// Relais de l'accès distant de McpHub.
//
// - `wss://RELAY_HOST/connect?hub=<hubId>` : le McpHub de l'utilisateur s'y connecte avec son
//   secret (en-tête Authorization). L'identifiant est dérivé du secret (26 premiers caractères
//   base32 de son SHA-256) : le relais refait le calcul, ne garde rien, et personne ne peut
//   prendre l'identifiant d'un autre. Une nouvelle connexion remplace l'ancienne.
// - `https://<hubId>.RELAY_HOST/...` : toute requête (Gemini, claude.ai, ChatGPT) est transmise
//   telle quelle au McpHub connecté, qui répond. Le relais ne lit ni ne garde rien d'autre :
//   l'OAuth, le consentement, le profil et le journal d'audit restent dans McpHub.
//
// Trame binaire (identique côté Rust, src/remote_access/relay.rs) : 4 octets de longueur de
// l'en-tête JSON (gros-boutiste), l'en-tête, puis le corps brut.

import { DurableObject } from "cloudflare:workers";

interface Env {
  HUB: DurableObjectNamespace<HubRelay>;
  RELAY_HOST?: string;
}

interface RequestHeader {
  t: "req";
  id: string;
  method: string;
  path: string;
  query: string;
  headers: Record<string, string>;
}

interface ResponseHeader {
  t: "res";
  id: string;
  status: number;
  headers: [string, string][];
}

interface Pending {
  resolve: (response: Response) => void;
  timer: ReturnType<typeof setTimeout>;
  authorize: boolean;
  bytes: number;
}

const HUB_ID = /^[a-z2-7]{26}$/;
const MAX_BODY = 16 * 1024 * 1024;
/// Corps admis hors de /mcp (inscription, jeton) : petit, pour ne pas saturer la mémoire.
const MAX_SMALL_BODY = 1024 * 1024;
const TIMEOUT_MS = 90_000;
const MAX_PENDING = 32;
/// Requêtes /authorize en attente au plus : elles ne bloquent jamais les appels /mcp.
const MAX_PENDING_AUTHORIZE = 4;
/// Octets de corps en transit au plus par hub (mémoire d'un Durable Object : 128 Mo).
const MAX_PENDING_BYTES = 48 * 1024 * 1024;
const HOP_HEADERS = new Set([
  "connection",
  "keep-alive",
  "transfer-encoding",
  "upgrade",
  "content-length",
  "te",
  "trailer",
  "proxy-authorization",
  "proxy-authenticate",
]);
const NULL_BODY_STATUS = new Set([101, 204, 205, 304]);

const text = (status: number, body: string): Response =>
  new Response(body, {
    status,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
    },
  });

const isWebSocket = (request: Request): boolean =>
  request.headers.get("Upgrade")?.toLowerCase() === "websocket";

function encodeFrame(header: RequestHeader, body: Uint8Array): Uint8Array {
  const head = new TextEncoder().encode(JSON.stringify(header));
  const frame = new Uint8Array(4 + head.byteLength + body.byteLength);
  new DataView(frame.buffer).setUint32(0, head.byteLength, false);
  frame.set(head, 4);
  frame.set(body, 4 + head.byteLength);
  return frame;
}

function decodeFrame(frame: Uint8Array): { header: ResponseHeader; body: Uint8Array } | null {
  if (frame.byteLength < 4) return null;
  const length = new DataView(frame.buffer, frame.byteOffset, frame.byteLength).getUint32(0, false);
  if (4 + length > frame.byteLength) return null;
  try {
    const header = JSON.parse(new TextDecoder().decode(frame.subarray(4, 4 + length))) as ResponseHeader;
    return { header, body: frame.slice(4 + length) };
  } catch {
    return null;
  }
}

/// Même calcul que McpHub (src/remote_access/relay.rs, hub_id_for).
async function hubIdFor(secret: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(secret)));
  const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
  let bits = 0;
  let acc = 0;
  let out = "";
  for (const byte of digest.subarray(0, 17)) {
    acc = (acc << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += alphabet[(acc >> bits) & 31];
    }
    acc &= (1 << bits) - 1;
  }
  return out.slice(0, 26);
}

function sameText(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const relayHost = env.RELAY_HOST?.trim().toLowerCase();
    if (!relayHost) return text(500, "relay not configured: RELAY_HOST is missing");
    const url = new URL(request.url);
    const host = url.hostname.toLowerCase();

    if (host === relayHost) {
      if (url.pathname !== "/connect") return text(404, "not found");
      const hub = url.searchParams.get("hub") ?? "";
      if (!HUB_ID.test(hub)) return text(400, "invalid hub id");
      if (!isWebSocket(request)) return text(426, "WebSocket upgrade required");
      return env.HUB.get(env.HUB.idFromName(hub)).fetch(request);
    }

    if (host.endsWith(`.${relayHost}`)) {
      const hub = host.slice(0, -(relayHost.length + 1));
      if (!HUB_ID.test(hub)) return text(404, "unknown hub");
      return env.HUB.get(env.HUB.idFromName(hub)).fetch(request);
    }

    return text(404, "not found");
  },
} satisfies ExportedHandler<Env>;

export class HubRelay extends DurableObject<Env> {
  private pending = new Map<string, Pending>();

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/connect" && isWebSocket(request) && url.searchParams.has("hub")) {
      return this.acceptHub(request);
    }
    return this.forward(request, url);
  }

  private async acceptHub(request: Request): Promise<Response> {
    const auth = request.headers.get("Authorization") ?? "";
    const secret = auth.startsWith("Bearer ") ? auth.slice(7).trim() : "";
    const hub = new URL(request.url).searchParams.get("hub") ?? "";
    if (secret.length < 32 || !sameText(await hubIdFor(secret), hub)) {
      return text(401, "unauthorized");
    }
    for (const old of this.ctx.getWebSockets()) old.close(4000, "replaced by a new connection");
    this.failAll(502, "McpHub reconnected, retry");
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    this.ctx.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  private async forward(request: Request, url: URL): Promise<Response> {
    const socket = this.ctx.getWebSockets()[0];
    if (!socket) return text(503, "McpHub is offline");
    const authorize = url.pathname.startsWith("/authorize");
    const pending = [...this.pending.values()];
    if (pending.length >= MAX_PENDING) return text(429, "too many requests in flight");
    if (authorize && pending.filter((p) => p.authorize).length >= MAX_PENDING_AUTHORIZE) {
      return text(429, "too many sign-ins in progress");
    }
    const limit = url.pathname.replace(/\/+$/, "") === "/mcp" ? MAX_BODY : MAX_SMALL_BODY;
    if (Number(request.headers.get("Content-Length") ?? "0") > limit) {
      return text(413, "payload too large");
    }
    const body = new Uint8Array(await request.arrayBuffer());
    if (body.byteLength > limit) return text(413, "payload too large");
    const inFlight = pending.reduce((sum, p) => sum + p.bytes, 0);
    if (inFlight + body.byteLength > MAX_PENDING_BYTES) return text(503, "relay busy, retry");

    const headers: Record<string, string> = {};
    request.headers.forEach((value, key) => {
      if (!HOP_HEADERS.has(key.toLowerCase())) headers[key.toLowerCase()] = value;
    });
    headers["host"] = url.hostname;
    const id = crypto.randomUUID();
    const frame = encodeFrame(
      { t: "req", id, method: request.method, path: url.pathname, query: url.search.slice(1), headers },
      body,
    );

    return new Promise<Response>((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(text(504, "McpHub did not answer in time"));
      }, TIMEOUT_MS);
      this.pending.set(id, { resolve, timer, authorize, bytes: body.byteLength });
      try {
        socket.send(frame);
      } catch {
        clearTimeout(timer);
        this.pending.delete(id);
        resolve(text(503, "McpHub is offline"));
      }
    });
  }

  async webSocketMessage(_ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") return;
    const decoded = decodeFrame(new Uint8Array(message));
    if (!decoded || decoded.header.t !== "res" || typeof decoded.header.id !== "string") return;
    const waiting = this.pending.get(decoded.header.id);
    if (!waiting) return;
    clearTimeout(waiting.timer);
    this.pending.delete(decoded.header.id);
    const status = Number.isInteger(decoded.header.status) ? decoded.header.status : 502;
    const headers = new Headers();
    for (const pair of decoded.header.headers ?? []) {
      if (Array.isArray(pair) && pair.length === 2 && !HOP_HEADERS.has(String(pair[0]).toLowerCase())) {
        headers.append(String(pair[0]), String(pair[1]));
      }
    }
    waiting.resolve(new Response(NULL_BODY_STATUS.has(status) ? null : decoded.body, { status, headers }));
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    this.failAll(502, "McpHub disconnected");
    if (code >= 1000 && code < 5000 && ![1004, 1005, 1006, 1015].includes(code)) {
      ws.close(code, reason);
    } else {
      ws.close();
    }
  }

  async webSocketError(): Promise<void> {
    this.failAll(502, "McpHub connection error");
  }

  private failAll(status: number, message: string): void {
    for (const [id, waiting] of this.pending) {
      clearTimeout(waiting.timer);
      waiting.resolve(text(status, message));
      this.pending.delete(id);
    }
  }
}
